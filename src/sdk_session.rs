//! Global SDK sessions shared by every region of a deployment. One SDK identity (uid) is one
//! Bilibili account. The official client revalidates it with `cache.login` once per launch and
//! then logs in to one server. So the regions of a deployment share the revalidated identity
//! instead of each sending its own `cache.login`, and an SDK refusal (for example a
//! risk-control block or a CAPTCHA) stops every region at once.
//!
//! With `global_login.state_directory`, the session and the refusal survive restarts. A restart
//! within the `id_token` lifetime sends no SDK request, and a refused identity is not retried
//! before `sdk_refusal_retry_seconds`. Processes sharing the directory (the service and
//! `global-account verify`) serialize `cache.login` with a lock file and read each other's
//! results. State files are private (0600 on Unix) and never logged. They hold the uid,
//! `id_token`, `mid` and a fingerprint of the access key, never the key itself.
use crate::{error::AppError, global_sdk::SdkAccount};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
    time::Duration,
};

const SCHEMA: u32 = 1;
const MAX_FILE_BYTES: u64 = 16 * 1024;
/// An `id_token` this close to its `exp` is revalidated instead of being sent.
const EXPIRY_MARGIN_SECONDS: i64 = 300;
const LOCK_POLL: Duration = Duration::from_millis(50);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
fn hex16(value: &str) -> String {
    Sha256::digest(value.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// `exp` of a JWT `id_token`, if it is one.
fn token_expiry(id_token: &str) -> Option<i64> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}
/// A JWT `id_token` that has expired or is about to. A token without `exp` does not expire
/// here: the game rejects it with `TOKEN_*`, which marks the session stale.
fn expiring(id_token: &str, now: DateTime<Utc>) -> bool {
    token_expiry(id_token).is_some_and(|exp| exp - EXPIRY_MARGIN_SECONDS <= now.timestamp())
}
fn header_safe(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && value.parse::<hyper::header::HeaderValue>().is_ok()
}
fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// An SDK refusal of the identity: the SDK's numeric code and when it was received.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Refusal {
    pub code: i64,
    pub at: DateTime<Utc>,
}

#[derive(Default)]
struct State {
    /// The identity as revalidated by the last successful `cache.login`.
    account: Option<SdkAccount>,
    validated_at: Option<DateTime<Utc>>,
    /// Incremented whenever `account` changes; a `TOKEN_*` signal only marks the generation
    /// whose `id_token` the failed session used.
    generation: u64,
    /// Set by `TOKEN_*` signals: the next login revalidates first.
    stale: bool,
    refusal: Option<Refusal>,
}
impl State {
    fn reusable(&self, now: DateTime<Utc>) -> Option<(SdkAccount, u64)> {
        if self.stale || self.refusal.is_some() {
            return None;
        }
        self.account
            .as_ref()
            .filter(|a| !expiring(&a.id_token, now))
            .map(|a| (a.clone(), self.generation))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Persisted {
    schema: u32,
    uid: String,
    access_key_fingerprint: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    mid: Option<String>,
    #[serde(default)]
    validated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    refusal: Option<Refusal>,
}

/// One SDK identity's session, shared by the accounts of every region that use it.
pub(crate) struct SdkSession {
    uid: String,
    access_key: String,
    access_key_fingerprint: String,
    file: Option<PathBuf>,
    /// Serializes `cache.login` of this identity across regions; a waiter reuses the result.
    pub login: tokio::sync::Mutex<()>,
    /// `cache.login` requests completed (whatever the outcome) in this process.
    attempts: AtomicU64,
    /// Successful `cache.login` requests in this process.
    revalidations: AtomicU64,
    state: Mutex<State>,
}
impl SdkSession {
    /// The revalidated identity and its generation, if it may be sent without another
    /// `cache.login`.
    pub fn reusable(&self, now: DateTime<Utc>) -> Option<(SdkAccount, u64)> {
        lock(&self.state).reusable(now)
    }
    /// The identity to revalidate from: the last revalidated one, if any.
    pub fn base(&self) -> Option<SdkAccount> {
        lock(&self.state).account.clone()
    }
    /// The refusal still in force, `retry` after it was received.
    pub fn refusal(&self, now: DateTime<Utc>, retry: chrono::Duration) -> Option<Refusal> {
        lock(&self.state).refusal.filter(|r| now < r.at + retry)
    }
    pub fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::Relaxed)
    }
    pub fn completed(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn revalidations(&self) -> u64 {
        self.revalidations.load(Ordering::Relaxed)
    }
    /// `refused`, `valid`, `stale`, `expired` or `none` (account status).
    pub fn label(&self, now: DateTime<Utc>) -> &'static str {
        let s = lock(&self.state);
        match (&s.refusal, &s.account) {
            (Some(_), _) => "refused",
            (None, None) => "none",
            (None, Some(_)) if s.stale => "stale",
            (None, Some(a)) if expiring(&a.id_token, now) => "expired",
            (None, Some(_)) => "valid",
        }
    }
    /// A `TOKEN_*` signal for a session logged in with `generation`: the next login of any
    /// region revalidates first, and the persisted `id_token` is dropped. A signal about an
    /// older generation (another region has revalidated since) changes nothing.
    pub fn mark_stale(&self, generation: u64) {
        let mut s = lock(&self.state);
        if s.generation != generation || s.stale {
            return;
        }
        s.stale = true;
        if s.account.is_some() {
            self.persist(&mut s);
        }
    }
    /// Records a successful `cache.login`; returns the new generation.
    pub fn validated(&self, account: SdkAccount) -> u64 {
        self.revalidations.fetch_add(1, Ordering::Relaxed);
        let mut s = lock(&self.state);
        s.account = Some(account);
        s.validated_at = Some(Utc::now());
        s.generation += 1;
        s.stale = false;
        s.refusal = None;
        self.persist(&mut s);
        s.generation
    }
    pub fn refused(&self, code: i64) {
        let mut s = lock(&self.state);
        s.account = None;
        s.generation += 1;
        s.stale = false;
        s.refusal = Some(Refusal {
            code,
            at: Utc::now(),
        });
        self.persist(&mut s);
    }
    /// Cross-process exclusion for one `cache.login` (with a state directory), waiting until
    /// `deadline`. Once held, the state is refreshed from the file, so a result another
    /// process wrote is reused instead of repeated.
    pub async fn exclusive(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Option<crate::file_lock::Exclusive>, AppError> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let Ok(handle) = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(name))
        else {
            tracing::warn!(
                error_code = "sdk_session_lock_unavailable",
                "Global SDK session lock file is unavailable; continuing without it"
            );
            return Ok(None);
        };
        let held = loop {
            let attempt = handle
                .try_clone()
                .map_err(std::fs::TryLockError::Error)
                .and_then(crate::file_lock::Exclusive::acquire);
            match attempt {
                Ok(held) => break held,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if tokio::time::Instant::now() + LOCK_POLL >= deadline {
                        return Err(AppError::Timeout);
                    }
                    tokio::time::sleep(LOCK_POLL).await;
                }
                Err(std::fs::TryLockError::Error(_)) => {
                    tracing::warn!(
                        error_code = "sdk_session_lock_unavailable",
                        "Global SDK session lock failed; continuing without it"
                    );
                    return Ok(None);
                }
            }
        };
        let mut s = lock(&self.state);
        self.merge(&mut s);
        Ok(Some(held))
    }
    /// Adopts a newer refusal or a newer valid session that another process wrote.
    fn merge(&self, s: &mut State) {
        let Some(disk) = self.file.as_deref().and_then(|f| self.read(f)) else {
            return;
        };
        let newer = |at: DateTime<Utc>| {
            s.validated_at.is_none_or(|v| at > v) && s.refusal.is_none_or(|r| at > r.at)
        };
        if let Some(refusal) = disk.refusal.filter(|r| newer(r.at)) {
            s.account = None;
            s.generation += 1;
            s.stale = false;
            s.refusal = Some(refusal);
        } else if let (Some(account), Some(at)) = (disk.account, disk.validated_at) {
            if newer(at) {
                s.account = Some(account);
                s.validated_at = Some(at);
                s.generation += 1;
                s.stale = false;
                s.refusal = None;
            }
        }
    }
    /// Written under the state lock, so updates of this process land in order. A newer
    /// refusal written by another process is kept rather than overwritten.
    fn persist(&self, s: &mut State) {
        let Some(file) = &self.file else {
            return;
        };
        self.merge(s);
        let account = s.account.as_ref().filter(|_| !s.stale);
        let persisted = Persisted {
            schema: SCHEMA,
            uid: self.uid.clone(),
            access_key_fingerprint: self.access_key_fingerprint.clone(),
            id_token: account.map(|a| a.id_token.clone()).unwrap_or_default(),
            mid: account.and_then(|a| a.mid.clone()),
            validated_at: account.and(s.validated_at),
            refusal: s.refusal,
        };
        if write_private(file, &persisted).is_err() {
            tracing::warn!(
                error_code = "sdk_session_persist_failed",
                "Global SDK session state could not be written; it stays in memory"
            );
        }
    }
    /// The file's state for this identity. A missing file is empty; an unreadable, malformed
    /// or foreign file is ignored with a warning. An `id_token` without a future `exp` is not
    /// adopted (a refusal in the same file still is).
    fn read(&self, path: &Path) -> Option<Loaded> {
        let bytes = match crate::accounts::read_private_file(
            path,
            MAX_FILE_BYTES,
            "unavailable",
            "invalid",
            "not private",
        ) {
            Ok(bytes) => bytes,
            Err(_) if !path.exists() => return None,
            Err(_) => {
                tracing::warn!(
                    error_code = "sdk_session_state_ignored",
                    "Global SDK session state is unreadable, too large or not private; ignored"
                );
                return None;
            }
        };
        let Some(p) = serde_json::from_slice::<Persisted>(&bytes)
            .ok()
            .filter(|p| {
                p.schema == SCHEMA
                    && p.uid == self.uid
                    && p.access_key_fingerprint == self.access_key_fingerprint
            })
        else {
            tracing::warn!(
                error_code = "sdk_session_state_ignored",
                "Global SDK session state is malformed or belongs to another identity; ignored"
            );
            return None;
        };
        let account = (p.refusal.is_none()
            && header_safe(&p.id_token)
            && token_expiry(&p.id_token).is_some()
            && !expiring(&p.id_token, Utc::now())
            && p.mid.as_deref().is_none_or(header_safe))
        .then(|| SdkAccount {
            uid: self.uid.clone(),
            access_key: self.access_key.clone(),
            id_token: p.id_token,
            mid: p.mid,
        });
        Some(Loaded {
            account,
            validated_at: p.validated_at,
            refusal: p.refusal,
        })
    }
}
struct Loaded {
    account: Option<SdkAccount>,
    validated_at: Option<DateTime<Utc>>,
    refusal: Option<Refusal>,
}

fn write_private(path: &Path, persisted: &Persisted) -> std::io::Result<()> {
    use std::io::Write;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let bytes = serde_json::to_vec(persisted).map_err(std::io::Error::other)?;
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = PathBuf::from(name);
    let result = private_options()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut f| f.write_all(&bytes).and_then(|_| f.sync_all()))
        .and_then(|_| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Deployment-wide registry: one [`SdkSession`] per SDK identity (uid and access key).
pub(crate) struct SdkSessions {
    directory: Option<PathBuf>,
    entries: Mutex<HashMap<(String, String), Weak<SdkSession>>>,
}
impl SdkSessions {
    /// Memory-only sessions, or sessions persisted in `directory` (created 0700 if missing).
    pub fn open(directory: Option<&Path>) -> Result<Arc<Self>, AppError> {
        if let Some(directory) = directory {
            let unavailable = || AppError::Config("global_login.state_directory is unavailable");
            if !directory.exists() {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(directory).map_err(|_| unavailable())?;
            }
            if !std::fs::metadata(directory)
                .map_err(|_| unavailable())?
                .is_dir()
            {
                return Err(unavailable());
            }
        }
        Ok(Arc::new(Self {
            directory: directory.map(Path::to_path_buf),
            entries: Mutex::new(HashMap::new()),
        }))
    }
    /// The shared session of `sdk`'s identity. A different access key for the same uid (a
    /// replaced identity file) is another session with its own state file.
    pub fn session(&self, sdk: &SdkAccount) -> Arc<SdkSession> {
        let fingerprint = hex16(&sdk.access_key);
        let key = (sdk.uid.clone(), fingerprint.clone());
        let mut entries = lock(&self.entries);
        entries.retain(|_, entry| entry.strong_count() > 0);
        if let Some(existing) = entries.get(&key).and_then(Weak::upgrade) {
            return existing;
        }
        let file = self.directory.as_ref().map(|d| {
            d.join(format!(
                "sdk-session-{}.json",
                hex16(&format!("{}\n{fingerprint}", sdk.uid))
            ))
        });
        let session = Arc::new(SdkSession {
            uid: sdk.uid.clone(),
            access_key: sdk.access_key.clone(),
            access_key_fingerprint: fingerprint,
            file,
            login: tokio::sync::Mutex::new(()),
            attempts: AtomicU64::new(0),
            revalidations: AtomicU64::new(0),
            state: Mutex::new(State::default()),
        });
        {
            let mut s = lock(&session.state);
            session.merge(&mut s);
        }
        entries.insert(key, Arc::downgrade(&session));
        session
    }
}
