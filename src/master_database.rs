//! Optional transactional PostgreSQL mirror of verified, generic Master documents.
use crate::master_registry::{self as registry, PublishedManifest, Scope};
use serde::{Deserialize, Serialize};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    ConnectOptions, PgPool, Row,
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Master database configuration")]
    Config,
    #[error("Master database secret unavailable")]
    Secret,
    #[error("Master source failed verification")]
    Snapshot,
    #[error("Master database operation failed")]
    Database,
    #[error("Master database operation timed out")]
    Timeout,
    #[error("Master database content conflicts with verified source")]
    Integrity,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password_env: String,
    pub root_certificate: Option<PathBuf>,
    /// Only literal loopback IPs may opt out of TLS, for local tunnels/test servers.
    #[serde(default)]
    pub plaintext_loopback: bool,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "retention")]
    pub keep_snapshots: usize,
}
fn port() -> u16 {
    5432
}
fn timeout() -> u64 {
    120
}
fn retention() -> usize {
    20
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    pub source: PathBuf,
    pub scope: Scope,
    pub database: Config,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub content_sha256: String,
    pub tables: usize,
    pub bytes: u64,
    pub changed: bool,
}
impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        let text = |s: &str| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control);
        if !text(&self.host)
            || self.host.contains(['/', '\\', '@', '?', '#', ' '])
            || !text(&self.database)
            || !text(&self.username)
            || self.port == 0
            || self.password_env.is_empty()
            || self.password_env.len() > 128
            || !self
                .password_env
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !(1..=600).contains(&self.timeout_seconds)
            || !(1..=10000).contains(&self.keep_snapshots)
            || (self.plaintext_loopback
                && !self
                    .host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()))
            || (self.plaintext_loopback && self.root_certificate.is_some())
        {
            return Err(Error::Config);
        }
        if let Some(path) = &self.root_certificate {
            let m = std::fs::symlink_metadata(path).map_err(|_| Error::Config)?;
            if !m.is_file() || m.file_type().is_symlink() || m.len() > 1024 * 1024 || m.len() == 0 {
                return Err(Error::Config);
            }
        }
        Ok(())
    }
    pub(crate) fn options(&self) -> Result<PgConnectOptions, Error> {
        self.validate()?;
        // SQLx's defaults read PG* options, including client key paths. Reject ambient
        // libpq configuration instead of accidentally importing another service's identity.
        if std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("PG")) {
            return Err(Error::Config);
        }
        let password = std::env::var(&self.password_env).map_err(|_| Error::Secret)?;
        if password.is_empty() || password.len() > 4096 {
            return Err(Error::Secret);
        }
        let mut options = PgConnectOptions::new_without_pgpass()
            .host(&self.host)
            .port(self.port)
            .database(&self.database)
            .username(&self.username)
            .password(&password)
            .application_name("sirius-master-database")
            .ssl_mode(if self.plaintext_loopback {
                PgSslMode::Disable
            } else {
                PgSslMode::VerifyFull
            })
            .options([
                (
                    "statement_timeout",
                    (self.timeout_seconds * 1000).to_string(),
                ),
                ("lock_timeout", (self.timeout_seconds * 1000).to_string()),
            ])
            .disable_statement_logging();
        if let Some(path) = &self.root_certificate {
            options = options.ssl_root_cert(path);
        }
        Ok(options)
    }
}
struct Snapshot {
    manifest: PublishedManifest,
    manifest_bytes: Vec<u8>,
    tables: Vec<(registry::File, Vec<u8>, serde_json::Value)>,
}
fn verified(source: &Path, scope: Scope) -> Result<Snapshot, Error> {
    let document = registry::manifest(source, None, scope.clone()).map_err(|_| Error::Snapshot)?;
    let manifest: PublishedManifest =
        serde_json::from_slice(&document.bytes).map_err(|_| Error::Snapshot)?;
    manifest.validate(&scope).map_err(|_| Error::Snapshot)?;
    let mut tables = Vec::new();
    for file in &manifest.files {
        let name = file.name.strip_suffix(".json").ok_or(Error::Snapshot)?;
        let data = registry::table(source, &manifest.snapshot, name, &file.sha256)
            .map_err(|_| Error::Snapshot)?;
        if data.bytes.len() as u64 != file.size {
            return Err(Error::Snapshot);
        }
        let value = serde_json::from_slice(&data.bytes).map_err(|_| Error::Snapshot)?;
        tables.push((file.clone(), data.bytes, value));
    }
    Ok(Snapshot {
        manifest,
        manifest_bytes: document.bytes,
        tables,
    })
}
/// Verify the entire pinned local snapshot before any database connection or mutation.
/// Cancellation rolls back the transaction; an uncertain commit is reconciled by content identity.
pub async fn publish(config: &Config, source: &Path, scope: Scope) -> Result<Receipt, Error> {
    config.validate()?;
    let source = source.to_owned();
    let snapshot = tokio::task::spawn_blocking(move || verified(&source, scope))
        .await
        .map_err(|_| Error::Snapshot)??;
    let options = config.options()?;
    let duration = Duration::from_secs(config.timeout_seconds);
    tokio::time::timeout(duration, async {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(duration)
            .connect_with(options)
            .await
            .map_err(|_| Error::Database)?;
        let result = publish_snapshot(&pool, config.keep_snapshots, snapshot).await;
        pool.close().await;
        result
    })
    .await
    .map_err(|_| Error::Timeout)?
}
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS public.sirius_master_snapshots (
 scope TEXT NOT NULL, content_hash TEXT NOT NULL, manifest BYTEA NOT NULL,
 touched BIGINT NOT NULL, PRIMARY KEY(scope, content_hash));
CREATE TABLE IF NOT EXISTS public.sirius_master_documents (
 scope TEXT NOT NULL, content_hash TEXT NOT NULL, name TEXT NOT NULL,
 sha256 TEXT NOT NULL, bytes BYTEA NOT NULL, document JSONB NOT NULL,
 PRIMARY KEY(scope, content_hash, name), FOREIGN KEY(scope, content_hash)
 REFERENCES public.sirius_master_snapshots(scope, content_hash) ON DELETE CASCADE);
CREATE INDEX IF NOT EXISTS sirius_master_documents_json ON public.sirius_master_documents USING GIN(document);
CREATE TABLE IF NOT EXISTS public.sirius_master_history (
 id BIGSERIAL PRIMARY KEY, scope TEXT NOT NULL, content_hash TEXT NOT NULL,
 published_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP);
CREATE INDEX IF NOT EXISTS sirius_master_history_scope ON public.sirius_master_history(scope,id DESC);
CREATE TABLE IF NOT EXISTS public.sirius_master_current (
 scope TEXT PRIMARY KEY, content_hash TEXT NOT NULL,
 FOREIGN KEY(scope, content_hash) REFERENCES public.sirius_master_snapshots(scope, content_hash));
";
async fn publish_snapshot(
    pool: &PgPool,
    keep: usize,
    snapshot: Snapshot,
) -> Result<Receipt, Error> {
    let mut tx = pool.begin().await.map_err(|_| Error::Database)?;
    // One transaction covers DDL, publication and retention. Shared database writers
    // serialize to prevent concurrent initialization and stale retention decisions.
    sqlx::query("SELECT pg_advisory_xact_lock(7369726975731200)")
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Database)?;
    sqlx::raw_sql(SCHEMA)
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Database)?;
    let scope = serde_json::to_string(&snapshot.manifest.scope).map_err(|_| Error::Snapshot)?;
    let hash = &snapshot.manifest.content_sha256;
    let current: Option<String> =
        sqlx::query_scalar("SELECT content_hash FROM public.sirius_master_current WHERE scope=$1")
            .bind(&scope)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| Error::Database)?;
    let existing: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT manifest FROM public.sirius_master_snapshots WHERE scope=$1 AND content_hash=$2",
    )
    .bind(&scope)
    .bind(hash)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| Error::Database)?;
    if let Some(bytes) = existing {
        let saved: PublishedManifest =
            serde_json::from_slice(&bytes).map_err(|_| Error::Integrity)?;
        saved
            .validate(&snapshot.manifest.scope)
            .map_err(|_| Error::Integrity)?;
        if saved.content_sha256 != *hash {
            return Err(Error::Integrity);
        }
        let rows = sqlx::query("SELECT name,sha256,bytes,document FROM public.sirius_master_documents WHERE scope=$1 AND content_hash=$2 ORDER BY name")
            .bind(&scope).bind(hash).fetch_all(&mut *tx).await.map_err(|_| Error::Database)?;
        if rows.len() != snapshot.tables.len() {
            return Err(Error::Integrity);
        }
        for (row, (file, bytes, value)) in rows.iter().zip(&snapshot.tables) {
            let name: String = row.try_get("name").map_err(|_| Error::Database)?;
            let sha: String = row.try_get("sha256").map_err(|_| Error::Database)?;
            let stored: Vec<u8> = row.try_get("bytes").map_err(|_| Error::Database)?;
            let json: serde_json::Value = row.try_get("document").map_err(|_| Error::Database)?;
            if name != file.name || sha != file.sha256 || &stored != bytes || &json != value {
                return Err(Error::Integrity);
            }
        }
    } else {
        sqlx::query("INSERT INTO public.sirius_master_snapshots(scope,content_hash,manifest,touched) VALUES($1,$2,$3,0)")
            .bind(&scope).bind(hash).bind(&snapshot.manifest_bytes).execute(&mut *tx).await.map_err(|_| Error::Database)?;
        for (file, bytes, value) in &snapshot.tables {
            sqlx::query("INSERT INTO public.sirius_master_documents(scope,content_hash,name,sha256,bytes,document) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(&scope).bind(hash).bind(&file.name).bind(&file.sha256).bind(bytes).bind(value)
                .execute(&mut *tx).await.map_err(|_| Error::Database)?;
        }
    }
    let changed = current.as_ref() != Some(hash);
    if changed {
        let sequence: i64 = sqlx::query_scalar("INSERT INTO public.sirius_master_history(scope,content_hash) VALUES($1,$2) RETURNING id")
            .bind(&scope).bind(hash).fetch_one(&mut *tx).await.map_err(|_| Error::Database)?;
        sqlx::query("UPDATE public.sirius_master_snapshots SET touched=$3 WHERE scope=$1 AND content_hash=$2")
            .bind(&scope).bind(hash).bind(sequence).execute(&mut *tx).await.map_err(|_| Error::Database)?;
        sqlx::query("INSERT INTO public.sirius_master_current(scope,content_hash) VALUES($1,$2) ON CONFLICT(scope) DO UPDATE SET content_hash=EXCLUDED.content_hash")
            .bind(&scope).bind(hash).execute(&mut *tx).await.map_err(|_| Error::Database)?;
    }
    sqlx::query("DELETE FROM public.sirius_master_snapshots WHERE scope=$1 AND content_hash<>$2 AND content_hash NOT IN (SELECT content_hash FROM public.sirius_master_snapshots WHERE scope=$1 ORDER BY touched DESC,content_hash LIMIT $3)")
        .bind(&scope).bind(hash).bind(keep as i64).execute(&mut *tx).await.map_err(|_| Error::Database)?;
    tx.commit().await.map_err(|_| Error::Database)?;
    Ok(Receipt {
        content_sha256: hash.clone(),
        tables: snapshot.tables.len(),
        bytes: snapshot.tables.iter().map(|(f, _, _)| f.size).sum(),
        changed,
    })
}
