//! Region-local account scheduling. Credentials never implement Debug or Serialize.
use crate::{
    config::{secret, Config},
    error::AppError,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    pub player_id_env: Option<String>,
    pub credential_env: Option<String>,
    pub credentials_file: Option<PathBuf>,
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolPolicy {
    pub failure_threshold: u32,
    pub cooldown_seconds: u64,
}
impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 2,
            cooldown_seconds: 30,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    player_id: String,
    credential: String,
}
struct Health {
    failures: u32,
    until: Option<Instant>,
    disabled: bool,
}
pub(crate) struct Account {
    pub name: String,
    pub player_id: String,
    pub credential: String,
    pub lock: tokio::sync::Mutex<()>,
    active: AtomicUsize,
    health: Mutex<Health>,
}
pub(crate) struct Lease {
    pub account: Arc<Account>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.account.active.fetch_sub(1, Ordering::Relaxed);
    }
}
#[derive(Serialize)]
pub struct AccountStatus {
    name: String,
    active_calls: usize,
    consecutive_failures: u32,
    cooldown_remaining_seconds: u64,
    disabled: bool,
}
pub(crate) struct Pool {
    entries: Vec<Arc<Account>>,
    next: usize,
    pub generation: u64,
}

pub fn validate(config: &Config) -> Result<(), AppError> {
    if config.accounts.len() > 64
        || !(1..=100).contains(&config.account_pool.failure_threshold)
        || !(1..=3600).contains(&config.account_pool.cooldown_seconds)
    {
        return Err(AppError::Config("invalid account pool bounds"));
    }
    if !config.accounts.is_empty()
        && (config.player_id_env.is_some() || config.player_credential_env.is_some())
    {
        return Err(AppError::Config(
            "accounts and legacy account references are mutually exclusive",
        ));
    }
    let mut names = HashSet::new();
    for c in &config.accounts {
        if c.name.is_empty()
            || c.name.len() > 64
            || !c
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || !names.insert(&c.name)
        {
            return Err(AppError::Config(
                "account names must be unique safe identifiers",
            ));
        }
        match (&c.player_id_env, &c.credential_env, &c.credentials_file) {
            (Some(id), Some(key), None) if !id.is_empty() && !key.is_empty() => {}
            (None, None, Some(path)) if !path.as_os_str().is_empty() => {}
            _ => {
                return Err(AppError::Config(
                    "each account requires paired environment references or one credentials file",
                ))
            }
        }
    }
    Ok(())
}
impl Account {
    pub fn available(&self) -> bool {
        let h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        !h.disabled && h.until.is_none_or(|until| until <= Instant::now())
    }
}
impl Pool {
    pub fn load(config: &Config, generation: u64) -> Result<Self, AppError> {
        validate(config)?;
        let legacy;
        let entries = if config.accounts.is_empty() {
            legacy = match (&config.player_id_env, &config.player_credential_env) {
                (Some(id), Some(key)) => vec![AccountConfig {
                    name: "default".into(),
                    player_id_env: Some(id.clone()),
                    credential_env: Some(key.clone()),
                    credentials_file: None,
                }],
                (None, None) => vec![],
                _ => return Err(AppError::Config("both account references required")),
            };
            &legacy
        } else {
            &config.accounts
        };
        let mut ids = HashSet::new();
        let mut loaded = Vec::new();
        for c in entries {
            let credentials = if let Some(path) = &c.credentials_file {
                let file = std::fs::File::open(path)
                    .map_err(|_| AppError::Config("account credentials file unavailable"))?;
                let metadata = file
                    .metadata()
                    .map_err(|_| AppError::Config("account credentials file unavailable"))?;
                if !metadata.is_file() || metadata.len() > 16384 {
                    return Err(AppError::Config("invalid account credentials file"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(AppError::Config(
                            "account credentials file must be private to its owner",
                        ));
                    }
                }
                let mut bytes = Vec::new();
                file.take(16385)
                    .read_to_end(&mut bytes)
                    .map_err(|_| AppError::Config("account credentials file unavailable"))?;
                if bytes.len() > 16384 {
                    return Err(AppError::Config("invalid account credentials file"));
                }
                serde_json::from_slice::<Credentials>(&bytes)
                    .map_err(|_| AppError::Config("invalid account credentials file"))?
            } else {
                Credentials {
                    player_id: secret(
                        c.player_id_env
                            .as_deref()
                            .ok_or(AppError::AccountUnavailable)?,
                    )?,
                    credential: secret(
                        c.credential_env
                            .as_deref()
                            .ok_or(AppError::AccountUnavailable)?,
                    )?,
                }
            };
            if [&credentials.player_id, &credentials.credential]
                .iter()
                .any(|v| v.trim().is_empty() || v.parse::<hyper::header::HeaderValue>().is_err())
                || !ids.insert(credentials.player_id.clone())
            {
                return Err(AppError::Config("invalid or duplicate account identity"));
            }
            loaded.push(Arc::new(Account {
                name: c.name.clone(),
                player_id: credentials.player_id,
                credential: credentials.credential,
                lock: tokio::sync::Mutex::new(()),
                active: AtomicUsize::new(0),
                health: Mutex::new(Health {
                    failures: 0,
                    until: None,
                    disabled: false,
                }),
            }));
        }
        Ok(Self {
            entries: loaded,
            next: 0,
            generation,
        })
    }
    pub fn select(&mut self, name: Option<&str>, private: bool) -> Result<Lease, AppError> {
        let index = if let Some(name) = name {
            self.entries
                .iter()
                .position(|a| a.name == name)
                .ok_or(AppError::NotFound)?
        } else if private {
            0
        } else {
            (0..self.entries.len())
                .map(|offset| (self.next + offset) % self.entries.len())
                .filter(|&i| self.entries[i].available())
                .min_by_key(|&i| self.entries[i].active.load(Ordering::Relaxed))
                .ok_or(AppError::AccountUnavailable)?
        };
        let account = self
            .entries
            .get(index)
            .filter(|a| a.available())
            .ok_or(AppError::AccountUnavailable)?
            .clone();
        self.next = (index + 1) % self.entries.len();
        account.active.fetch_add(1, Ordering::Relaxed);
        Ok(Lease { account })
    }
    pub fn status(&self) -> Vec<AccountStatus> {
        self.entries
            .iter()
            .map(|a| {
                let h = a.health.lock().unwrap_or_else(|e| e.into_inner());
                AccountStatus {
                    name: a.name.clone(),
                    active_calls: a.active.load(Ordering::Relaxed),
                    consecutive_failures: h.failures,
                    cooldown_remaining_seconds: h
                        .until
                        .map(|t| {
                            let remaining = t.saturating_duration_since(Instant::now());
                            remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
                        })
                        .unwrap_or(0),
                    disabled: h.disabled,
                }
            })
            .collect()
    }
}
impl Lease {
    pub fn report(&self, result: &Result<serde_json::Value, AppError>, policy: &PoolPolicy) {
        let mut h = self
            .account
            .health
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(_) => {
                h.failures = 0;
                h.until = None;
            }
            Err(AppError::Grpc(7 | 16)) => {
                h.disabled = true;
                h.failures = h.failures.saturating_add(1);
            }
            Err(
                AppError::Timeout
                | AppError::Transport
                | AppError::Protocol
                | AppError::Grpc(8 | 13 | 14),
            ) => {
                h.failures = h.failures.saturating_add(1);
                if h.failures >= policy.failure_threshold {
                    h.until = Some(Instant::now() + Duration::from_secs(policy.cooldown_seconds));
                }
            }
            _ => {}
        }
    }
}
