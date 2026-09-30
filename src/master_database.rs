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
    #[error("Master database document not found")]
    NotFound,
    #[error("invalid Master database read request")]
    InvalidRequest,
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
    /// Publication, import and migration deadline (also caps the default read deadline).
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    /// Deadline for registry/HTTP reads (default: the smaller of timeout_seconds and 30).
    /// Independent of timeout_seconds, which keeps bounding publication, import and migration.
    #[serde(default)]
    pub read_timeout_seconds: Option<u64>,
    #[serde(default = "retention")]
    pub keep_snapshots: usize,
    /// Read pool size (registry and HTTP reads). Writers always use one connection:
    /// publication and migration are single serialized transactions.
    #[serde(default = "read_connections")]
    pub max_read_connections: u32,
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
fn read_connections() -> u32 {
    4
}
/// Waiting for a pooled/new read connection fails fast; the read deadline still bounds the whole read.
const READ_ACQUIRE_LIMIT: Duration = Duration::from_secs(5);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    pub source: PathBuf,
    #[serde(deserialize_with = "crate::master_registry::config_scope")]
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
            || self
                .read_timeout_seconds
                .is_some_and(|s| !(1..=600).contains(&s))
            || !(1..=10000).contains(&self.keep_snapshots)
            || !(1..=64).contains(&self.max_read_connections)
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
    pub(crate) fn read_timeout(&self) -> Duration {
        Duration::from_secs(
            self.read_timeout_seconds
                .unwrap_or(self.timeout_seconds.min(30)),
        )
    }
    /// Writer options: server statement/lock deadlines follow timeout_seconds.
    pub(crate) fn options(&self) -> Result<PgConnectOptions, Error> {
        self.options_named("sirius-master-database")
    }
    pub(crate) fn options_named(&self, application: &str) -> Result<PgConnectOptions, Error> {
        self.connect_options(application, Duration::from_secs(self.timeout_seconds))
    }
    /// Reader options: a read the client abandons, or one queued behind a lock, is also cancelled
    /// by the server at the read deadline instead of running on for the writer budget.
    pub(crate) fn read_options(&self) -> Result<PgConnectOptions, Error> {
        self.connect_options("sirius-master-database", self.read_timeout())
    }
    fn connect_options(
        &self,
        application: &str,
        deadline: Duration,
    ) -> Result<PgConnectOptions, Error> {
        self.validate()?;
        // SQLx's defaults read PG* options, including client certificate/key paths. Reject ambient
        // libpq configuration that would survive the explicit settings below instead of
        // accidentally importing another service's identity, trust roots or server options.
        if ambient_libpq_configuration(|name| std::env::var_os(name).is_some()) {
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
            .application_name(application)
            .ssl_mode(if self.plaintext_loopback {
                PgSslMode::Disable
            } else {
                PgSslMode::VerifyFull
            })
            .options([
                ("statement_timeout", deadline.as_millis().to_string()),
                ("lock_timeout", deadline.as_millis().to_string()),
            ])
            .disable_statement_logging();
        if let Some(path) = &self.root_certificate {
            options = options.ssl_root_cert(path);
        }
        Ok(options)
    }
}
/// libpq environment variables that SQLx 0.9 `PgConnectOptions::new_without_pgpass()` reads and
/// whose values would survive the explicit configuration in `connect_options`: a trust root when
/// none is configured, a client certificate/key, and server options appended to our own.
///
/// SQLx also reads PGHOST, PGHOSTADDR, PGPORT, PGUSER, PGPASSWORD, PGDATABASE, PGSSLMODE and
/// PGAPPNAME, but every one of those is unconditionally replaced by an explicit value, and
/// PGPASSFILE is only consulted by the password-file lookup this transport never uses. Server
/// installation variables (PGDATA, PGBIN, PGROOT, ...) are not client configuration at all.
/// Hosts with PostgreSQL tooling (including GitHub Windows runners, which set PGUSER and
/// PGPASSWORD) therefore keep working. Review this list whenever SQLx is upgraded.
const INHERITED_LIBPQ_VARIABLES: [&str; 4] =
    ["PGSSLROOTCERT", "PGSSLCERT", "PGSSLKEY", "PGOPTIONS"];
pub(crate) fn ambient_libpq_configuration(is_set: impl Fn(&str) -> bool) -> bool {
    INHERITED_LIBPQ_VARIABLES.iter().any(|name| is_set(name))
}
struct Snapshot {
    manifest: PublishedManifest,
    manifest_bytes: Vec<u8>,
    tables: Vec<(registry::File, Vec<u8>, serde_json::Value)>,
}
fn verified(source: &Path, scope: Scope) -> Result<Snapshot, Error> {
    verified_snapshot(source, scope, None)
}
fn verified_snapshot(source: &Path, scope: Scope, id: Option<&str>) -> Result<Snapshot, Error> {
    let document = registry::manifest(source, id, scope.clone()).map_err(|_| Error::Snapshot)?;
    let manifest: PublishedManifest =
        serde_json::from_slice(&document.bytes).map_err(|_| Error::Snapshot)?;
    manifest.validate(&scope).map_err(|_| Error::Snapshot)?;
    let mut tables = Vec::new();
    for file in &manifest.files {
        let name = file.name.strip_suffix(".json").ok_or(Error::Snapshot)?;
        let data = registry::table(source, scope.region, &manifest.snapshot, name, &file.sha256)
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
/// Verify local CURRENT without loading database credentials or establishing a connection.
pub async fn verify_current(source: &Path, scope: Scope) -> Result<Receipt, Error> {
    let source = source.to_owned();
    tokio::task::spawn_blocking(move || {
        let snapshot = verified(&source, scope)?;
        Ok(Receipt {
            content_sha256: snapshot.manifest.content_sha256,
            tables: snapshot.tables.len(),
            bytes: snapshot.tables.iter().map(|(f, _, _)| f.size).sum(),
            changed: false,
        })
    })
    .await
    .map_err(|_| Error::Snapshot)?
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
// 1.3.0 added nullable per-event metadata to history (no backfill). Tables created by 1.2.x
// are upgraded once: ALTER TABLE takes an exclusive lock even when every column exists, so it
// only runs while a column is missing and steady-state publication never blocks history reads.
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
 published_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
 version TEXT, resource_version TEXT, file_count BIGINT, total_size BIGINT);
CREATE INDEX IF NOT EXISTS sirius_master_history_scope ON public.sirius_master_history(scope,id DESC);
DO $$ BEGIN
 IF (SELECT count(*) FROM pg_attribute WHERE attrelid='public.sirius_master_history'::regclass
  AND attname IN ('version','resource_version','file_count','total_size') AND NOT attisdropped) < 4 THEN
  ALTER TABLE public.sirius_master_history ADD COLUMN IF NOT EXISTS version TEXT,
   ADD COLUMN IF NOT EXISTS resource_version TEXT, ADD COLUMN IF NOT EXISTS file_count BIGINT,
   ADD COLUMN IF NOT EXISTS total_size BIGINT;
 END IF;
END $$;
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
    let receipt = store_snapshot(&mut tx, keep, snapshot, None).await?;
    tx.commit().await.map_err(|_| Error::Database)?;
    Ok(receipt)
}

async fn store_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    keep: usize,
    snapshot: Snapshot,
    publication: Option<Option<String>>,
) -> Result<Receipt, Error> {
    let scope = serde_json::to_string(&snapshot.manifest.scope).map_err(|_| Error::Snapshot)?;
    let hash = &snapshot.manifest.content_sha256;
    let current: Option<String> =
        sqlx::query_scalar("SELECT content_hash FROM public.sirius_master_current WHERE scope=$1")
            .bind(&scope)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|_| Error::Database)?;
    let existing: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT manifest FROM public.sirius_master_snapshots WHERE scope=$1 AND content_hash=$2",
    )
    .bind(&scope)
    .bind(hash)
    .fetch_optional(&mut **tx)
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
            .bind(&scope).bind(hash).fetch_all(&mut **tx).await.map_err(|_| Error::Database)?;
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
        // Asset-version provenance is outside content identity: identical tables may gain a
        // recorded asset version. Keep the served manifest current without losing a known one.
        if snapshot.manifest.resource_version.is_some()
            && saved.resource_version != snapshot.manifest.resource_version
        {
            sqlx::query("UPDATE public.sirius_master_snapshots SET manifest=$3 WHERE scope=$1 AND content_hash=$2")
                .bind(&scope).bind(hash).bind(&snapshot.manifest_bytes).execute(&mut **tx).await.map_err(|_| Error::Database)?;
        }
    } else {
        sqlx::query("INSERT INTO public.sirius_master_snapshots(scope,content_hash,manifest,touched) VALUES($1,$2,$3,0)")
            .bind(&scope).bind(hash).bind(&snapshot.manifest_bytes).execute(&mut **tx).await.map_err(|_| Error::Database)?;
        for (file, bytes, value) in &snapshot.tables {
            sqlx::query("INSERT INTO public.sirius_master_documents(scope,content_hash,name,sha256,bytes,document) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(&scope).bind(hash).bind(&file.name).bind(&file.sha256).bind(bytes).bind(value)
                .execute(&mut **tx).await.map_err(|_| Error::Database)?;
        }
    }
    let bytes: u64 = snapshot.tables.iter().map(|(f, _, _)| f.size).sum();
    let changed = publication.is_some() || current.as_ref() != Some(hash);
    if changed {
        // Events are immutable: each records the provenance known when it was written.
        let total = i64::try_from(bytes).map_err(|_| Error::Snapshot)?;
        let sequence: i64 = sqlx::query_scalar("INSERT INTO public.sirius_master_history(scope,content_hash,published_at,version,resource_version,file_count,total_size) VALUES($1,$2,COALESCE($3::text::timestamptz,CURRENT_TIMESTAMP),$4,$5,$6,$7) RETURNING id")
            .bind(&scope).bind(hash).bind(publication.flatten()).bind(&snapshot.manifest.version)
            .bind(snapshot.manifest.resource_version.as_deref()).bind(snapshot.tables.len() as i64).bind(total)
            .fetch_one(&mut **tx).await.map_err(|_| Error::Database)?;
        sqlx::query("UPDATE public.sirius_master_snapshots SET touched=$3 WHERE scope=$1 AND content_hash=$2")
            .bind(&scope).bind(hash).bind(sequence).execute(&mut **tx).await.map_err(|_| Error::Database)?;
        sqlx::query("INSERT INTO public.sirius_master_current(scope,content_hash) VALUES($1,$2) ON CONFLICT(scope) DO UPDATE SET content_hash=EXCLUDED.content_hash")
            .bind(&scope).bind(hash).execute(&mut **tx).await.map_err(|_| Error::Database)?;
    }
    sqlx::query("DELETE FROM public.sirius_master_snapshots WHERE scope=$1 AND content_hash<>$2 AND content_hash NOT IN (SELECT content_hash FROM public.sirius_master_snapshots WHERE scope=$1 ORDER BY touched DESC,content_hash LIMIT $3)")
        .bind(&scope).bind(hash).bind(keep as i64).execute(&mut **tx).await.map_err(|_| Error::Database)?;
    Ok(Receipt {
        content_sha256: hash.clone(),
        tables: snapshot.tables.len(),
        bytes,
        changed,
    })
}

/// One-time migration of committed file publications into an empty database scope.
#[derive(Debug, Serialize, Deserialize)]
pub struct MigrationReceipt {
    pub head: String,
    pub source_sha256: String,
    pub publications: usize,
    pub legacy_boundary: bool,
    /// The plan ended at the file retention boundary: only the retained window was
    /// migrated. Absent means false. The plan digest already covers it, so no column.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retention_boundary: bool,
    pub changed: bool,
}
const MIGRATION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS public.sirius_master_migrations (
 scope TEXT PRIMARY KEY, source_hash TEXT NOT NULL, head TEXT NOT NULL,
 publications BIGINT NOT NULL, legacy_boundary BOOLEAN NOT NULL);
";

pub async fn migrate_history(
    config: &Config,
    source: &Path,
    scope: Scope,
) -> Result<MigrationReceipt, Error> {
    config.validate()?;
    let root = source.to_owned();
    let expected_scope = scope.clone();
    // Verify each source payload before opening a connection, retaining one snapshot
    // at a time. CURRENT is read once; subsequent installations cannot change the plan.
    let history = tokio::task::spawn_blocking(move || {
        let history = registry::committed_history(&root, expected_scope.clone())
            .map_err(|_| Error::Snapshot)?;
        for entry in &history.entries {
            let snapshot = verified_snapshot(&root, expected_scope.clone(), Some(&entry.snapshot))?;
            if snapshot.manifest.content_sha256 != entry.content_sha256 {
                return Err(Error::Snapshot);
            }
        }
        Ok::<_, Error>(history)
    })
    .await
    .map_err(|_| Error::Snapshot)??;
    let source_sha256 = migration_plan_digest(&history)?;
    let options = config.options()?;
    let duration = Duration::from_secs(config.timeout_seconds);
    tokio::time::timeout(duration, async {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(duration)
            .connect_with(options)
            .await
            .map_err(|_| Error::Database)?;
        let result =
            migrate_transaction(&pool, config.keep_snapshots, source, history, source_sha256).await;
        pool.close().await;
        result
    })
    .await
    .map_err(|_| Error::Timeout)?
}

/// Hash of a migration source plan, stored as the durable receipt in
/// `sirius_master_migrations.source_hash` and compared on every replay. This is a frozen format:
/// exactly the 1.2 serialization of `registry::History`. It must not follow History's public
/// shape (1.3.0 added `resource_version` to entries), or replaying `master-db-migrate` on a scope
/// migrated by an earlier release would be refused as an integrity conflict. The 1.3.0
/// `retention_boundary` is omitted when false, so plans of never-pruned directories keep
/// their 1.2 digest.
pub(crate) fn migration_plan_digest(history: &registry::History) -> Result<String, Error> {
    #[derive(Serialize)]
    struct PlanV1<'a> {
        schema_version: u32,
        scope: &'a Scope,
        head: &'a str,
        entries: Vec<PlanEntryV1<'a>>,
        has_more: bool,
        next_before: Option<&'a str>,
        legacy_boundary: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        retention_boundary: bool,
    }
    #[derive(Serialize)]
    struct PlanEntryV1<'a> {
        snapshot: &'a str,
        version: &'a str,
        content_sha256: &'a str,
        published_at: Option<chrono::DateTime<chrono::Utc>>,
        file_count: usize,
        total_size: u64,
    }
    let plan = PlanV1 {
        schema_version: history.schema_version,
        scope: &history.scope,
        head: &history.head,
        entries: history
            .entries
            .iter()
            .map(|e| PlanEntryV1 {
                snapshot: &e.snapshot,
                version: &e.version,
                content_sha256: &e.content_sha256,
                published_at: e.published_at,
                file_count: e.file_count,
                total_size: e.total_size,
            })
            .collect(),
        has_more: history.has_more,
        next_before: history.next_before.as_deref(),
        legacy_boundary: history.legacy_boundary,
        retention_boundary: history.retention_boundary,
    };
    Ok(registry::digest(
        &serde_json::to_vec(&plan).map_err(|_| Error::Snapshot)?,
    ))
}

async fn migrate_transaction(
    pool: &PgPool,
    keep: usize,
    source: &Path,
    history: registry::History,
    source_sha256: String,
) -> Result<MigrationReceipt, Error> {
    let mut tx = pool.begin().await.map_err(|_| Error::Database)?;
    sqlx::query("SELECT pg_advisory_xact_lock(7369726975731200)")
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Database)?;
    sqlx::raw_sql(SCHEMA)
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Database)?;
    sqlx::raw_sql(MIGRATION_SCHEMA)
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Database)?;
    let scope = serde_json::to_string(&history.scope).map_err(|_| Error::Snapshot)?;
    let saved = sqlx::query("SELECT source_hash,head,publications,legacy_boundary FROM public.sirius_master_migrations WHERE scope=$1")
        .bind(&scope).fetch_optional(&mut *tx).await.map_err(|_| Error::Database)?;
    let mut receipt = MigrationReceipt {
        head: history.head.clone(),
        source_sha256,
        publications: history.entries.len(),
        legacy_boundary: history.legacy_boundary,
        retention_boundary: history.retention_boundary,
        changed: false,
    };
    if let Some(row) = saved {
        if row
            .try_get::<String, _>("source_hash")
            .map_err(|_| Error::Database)?
            != receipt.source_sha256
            || row
                .try_get::<String, _>("head")
                .map_err(|_| Error::Database)?
                != receipt.head
            || row
                .try_get::<i64, _>("publications")
                .map_err(|_| Error::Database)?
                != receipt.publications as i64
            || row
                .try_get::<bool, _>("legacy_boundary")
                .map_err(|_| Error::Database)?
                != receipt.legacy_boundary
        {
            return Err(Error::Integrity);
        }
        // A durable receipt acknowledges the original migration even after later
        // publications or retention. Retrying must never rewind the current pointer.
        tx.commit().await.map_err(|_| Error::Database)?;
        return Ok(receipt);
    }
    let occupied: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.sirius_master_snapshots WHERE scope=$1 UNION ALL SELECT 1 FROM public.sirius_master_current WHERE scope=$1 UNION ALL SELECT 1 FROM public.sirius_master_history WHERE scope=$1)")
        .bind(&scope).fetch_one(&mut *tx).await.map_err(|_| Error::Database)?;
    if occupied {
        return Err(Error::Integrity);
    }
    for entry in history.entries.into_iter().rev() {
        let root = source.to_owned();
        let scope = history.scope.clone();
        let id = entry.snapshot;
        let snapshot =
            tokio::task::spawn_blocking(move || verified_snapshot(&root, scope, Some(&id)))
                .await
                .map_err(|_| Error::Snapshot)??;
        if snapshot.manifest.content_sha256 != entry.content_sha256 {
            return Err(Error::Snapshot);
        }
        store_snapshot(
            &mut tx,
            keep,
            snapshot,
            Some(entry.published_at.map(|t| t.to_rfc3339())),
        )
        .await?;
    }
    sqlx::query("INSERT INTO public.sirius_master_migrations(scope,source_hash,head,publications,legacy_boundary) VALUES($1,$2,$3,$4,$5)")
        .bind(&scope).bind(&receipt.source_sha256).bind(&receipt.head).bind(receipt.publications as i64)
        .bind(receipt.legacy_boundary).execute(&mut *tx).await.map_err(|_| Error::Database)?;
    tx.commit().await.map_err(|_| Error::Database)?;
    receipt.changed = true;
    Ok(receipt)
}

/// Shared bounded read pool. Construction performs no network I/O.
#[derive(Clone)]
pub struct Reader {
    pool: std::sync::Arc<tokio::sync::OnceCell<PgPool>>,
    options: PgConnectOptions,
    timeout: Duration,
    acquire: Duration,
    connections: u32,
}
#[derive(Serialize)]
pub struct HistoryEntry {
    pub sequence: String,
    pub content_sha256: String,
    pub retained: bool,
    pub published_at: chrono::DateTime<chrono::Utc>,
    /// Event metadata, null on events written before 1.3.0 or by a 1.2.x writer (no backfill).
    /// With a version, a null `resource_version` means no asset version was recorded.
    pub version: Option<String>,
    pub resource_version: Option<String>,
    pub file_count: Option<u64>,
    pub total_size: Option<u64>,
}
/// One history row as read from the database, before validation.
pub(crate) struct HistoryRow {
    pub id: i64,
    pub content_hash: String,
    pub retained: bool,
    pub published_at: Option<String>,
    pub version: Option<String>,
    pub resource_version: Option<String>,
    pub file_count: Option<i64>,
    pub total_size: Option<i64>,
}
/// Validate a stored event. Invalid rows fail closed without echoing any stored value.
pub(crate) fn history_entry(row: HistoryRow) -> Result<HistoryEntry, Error> {
    let published_at = row
        .published_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .ok_or(Error::Integrity)?
        .with_timezone(&chrono::Utc);
    let (file_count, total_size) = match (&row.version, row.file_count, row.total_size) {
        (None, None, None) => (None, None),
        (Some(_), Some(count), Some(total))
            if count >= 1 && count <= total && total as u64 <= registry::MAX_TOTAL =>
        {
            (Some(count as u64), Some(total as u64))
        }
        _ => return Err(Error::Integrity),
    };
    if row.id <= 0
        || !registry::hash_valid(&row.content_hash)
        || (row.resource_version.is_some() && row.version.is_none())
        || [&row.version, &row.resource_version]
            .into_iter()
            .flatten()
            .any(|v| !crate::master::safe_version(v))
    {
        return Err(Error::Integrity);
    }
    Ok(HistoryEntry {
        sequence: row.id.to_string(),
        content_sha256: row.content_hash,
        retained: row.retained,
        published_at,
        version: row.version,
        resource_version: row.resource_version,
        file_count,
        total_size,
    })
}
#[derive(Serialize)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next_before: Option<String>,
}
impl Reader {
    pub fn new(config: &Config) -> Result<Self, Error> {
        let timeout = config.read_timeout();
        Ok(Self {
            pool: Default::default(),
            options: config.read_options()?,
            timeout,
            acquire: timeout.min(READ_ACQUIRE_LIMIT),
            connections: config.max_read_connections,
        })
    }
    /// Read deadline, pool acquire limit and server options (no credentials). No network I/O.
    #[cfg(test)]
    pub(crate) async fn limits(&self) -> (Duration, Duration, Option<String>) {
        (
            self.timeout,
            self.pool().await.options().get_acquire_timeout(),
            self.options.get_options().map(str::to_owned),
        )
    }
    async fn pool(&self) -> &PgPool {
        self.pool
            .get_or_init(|| async {
                PgPoolOptions::new()
                    .max_connections(self.connections)
                    .acquire_timeout(self.acquire)
                    .connect_lazy_with(self.options.clone())
            })
            .await
    }
    pub async fn document(
        &self,
        scope: &Scope,
        hash: Option<&str>,
        table: Option<&str>,
    ) -> Result<registry::Document, Error> {
        if !scope.region.master_supported()
            || hash.is_some_and(|h| !registry::hash_valid(h))
            || table.is_some_and(|t| hash.is_none() || !crate::master::safe_component(t))
        {
            return Err(Error::InvalidRequest);
        }
        tokio::time::timeout(self.timeout, self.document_inner(scope, hash, table))
            .await
            .map_err(|_| Error::Timeout)?
    }
    async fn document_inner(
        &self,
        scope: &Scope,
        hash: Option<&str>,
        table: Option<&str>,
    ) -> Result<registry::Document, Error> {
        let key = serde_json::to_string(scope).map_err(|_| Error::Config)?;
        let mut tx = self
            .pool()
            .await
            .begin()
            .await
            .map_err(|_| Error::Database)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::Database)?;
        // Bound bytes on the database side, before decoding/allocating a response.
        let row=sqlx::query("SELECT content_hash, CASE WHEN octet_length(manifest)<=1048576 THEN manifest END AS manifest FROM public.sirius_master_snapshots WHERE scope=$1 AND content_hash=COALESCE($2,(SELECT content_hash FROM public.sirius_master_current WHERE scope=$1))")
            .bind(&key).bind(hash).fetch_optional(&mut *tx).await.map_err(|_| Error::Database)?.ok_or(Error::NotFound)?;
        let selected: String = row.try_get("content_hash").map_err(|_| Error::Database)?;
        let manifest_bytes: Option<Vec<u8>> =
            row.try_get("manifest").map_err(|_| Error::Database)?;
        let manifest_bytes = manifest_bytes.ok_or(Error::Integrity)?;
        let manifest: PublishedManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|_| Error::Integrity)?;
        manifest.validate(scope).map_err(|_| Error::Integrity)?;
        if manifest.content_sha256 != selected || hash.is_some_and(|h| h != selected) {
            return Err(Error::Integrity);
        }
        let bytes = if let Some(table) = table {
            let name = format!("{table}.json");
            let expected = manifest
                .files
                .iter()
                .find(|f| f.name == name)
                .ok_or(Error::NotFound)?;
            let row=sqlx::query("SELECT sha256, CASE WHEN octet_length(bytes)<=67108864 THEN bytes END AS bytes FROM public.sirius_master_documents WHERE scope=$1 AND content_hash=$2 AND name=$3")
                .bind(&key).bind(&selected).bind(name).fetch_optional(&mut *tx).await.map_err(|_| Error::Database)?.ok_or(Error::Integrity)?;
            let digest: String = row.try_get("sha256").map_err(|_| Error::Database)?;
            let data: Option<Vec<u8>> = row.try_get("bytes").map_err(|_| Error::Database)?;
            let data = data.ok_or(Error::Integrity)?;
            if data.len() as u64 != expected.size
                || digest != expected.sha256
                || registry::digest(&data) != expected.sha256
            {
                return Err(Error::Integrity);
            }
            crate::master::validate_json(&data).map_err(|_| Error::Integrity)?;
            data
        } else {
            manifest_bytes
        };
        tx.commit().await.map_err(|_| Error::Database)?;
        Ok(registry::Document {
            etag: format!("\"{}\"", registry::digest(&bytes)),
            version: manifest.version,
            bytes,
        })
    }
    pub async fn history(
        &self,
        scope: &Scope,
        limit: usize,
        before: Option<i64>,
    ) -> Result<HistoryPage, Error> {
        if !scope.region.master_supported()
            || !(1..=200).contains(&limit)
            || before.is_some_and(|n| n <= 0)
        {
            return Err(Error::InvalidRequest);
        }
        let key = serde_json::to_string(scope).map_err(|_| Error::Config)?;
        tokio::time::timeout(self.timeout,async {
            // Metadata columns are read through to_jsonb, so a table not yet upgraded from 1.2
            // reads them as null. Oversized text is cut one byte past the version bound, so it is
            // still rejected rather than silently truncated; the time is rendered in UTC.
            let rows=sqlx::query("SELECT h.id,h.content_hash,EXISTS(SELECT 1 FROM public.sirius_master_snapshots s WHERE s.scope=h.scope AND s.content_hash=h.content_hash) AS retained,to_char(h.published_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS published_at,substr(to_jsonb(h)->>'version',1,257) AS version,substr(to_jsonb(h)->>'resource_version',1,257) AS resource_version,(to_jsonb(h)->>'file_count')::bigint AS file_count,(to_jsonb(h)->>'total_size')::bigint AS total_size FROM public.sirius_master_history h WHERE h.scope=$1 AND ($2::bigint IS NULL OR h.id<$2) ORDER BY h.id DESC LIMIT $3")
                .bind(key).bind(before).bind((limit+1) as i64).fetch_all(self.pool().await).await.map_err(|_| Error::Database)?;
            let more=rows.len()>limit;
            let mut entries=Vec::new();
            for row in rows.into_iter().take(limit) {
                let get=|_:sqlx::Error| Error::Database;
                entries.push(history_entry(HistoryRow{
                    id:row.try_get("id").map_err(get)?,
                    content_hash:row.try_get("content_hash").map_err(get)?,
                    retained:row.try_get("retained").map_err(get)?,
                    published_at:row.try_get("published_at").map_err(get)?,
                    version:row.try_get("version").map_err(get)?,
                    resource_version:row.try_get("resource_version").map_err(get)?,
                    file_count:row.try_get("file_count").map_err(get)?,
                    total_size:row.try_get("total_size").map_err(get)?,
                })?);
            }
            let next_before=if more {entries.last().map(|e|e.sequence.clone())} else {None};
            Ok(HistoryPage{entries,next_before})
        }).await.map_err(|_| Error::Timeout)?
    }
}
