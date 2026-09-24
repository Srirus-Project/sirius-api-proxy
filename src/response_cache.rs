//! Optional response storage. Callers supply credential/protocol-scoped digests, never raw keys.
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Announcements,
    Announcement,
    EventRankings,
    SongRankings,
    ChallengeRankings,
}
impl Route {
    fn from_rpc(route: &str) -> Option<Self> {
        use crate::routes::*;
        match route {
            ANNOUNCEMENTS => Some(Self::Announcements),
            ANNOUNCEMENT => Some(Self::Announcement),
            EVENT_RANKING => Some(Self::EventRankings),
            MUSIC_RANKING => Some(Self::SongRankings),
            CHALLENGE_RANKING => Some(Self::ChallengeRankings),
            _ => None,
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum Config {
    #[default]
    Disabled,
    Memory {
        ttl_ms: u64,
        #[serde(default)]
        route_ttl_ms: BTreeMap<Route, u64>,
        max_entries: usize,
        max_bytes: usize,
        max_entry_bytes: usize,
    },
    Redis {
        url_env: String,
        namespace: String,
        ttl_ms: u64,
        #[serde(default)]
        route_ttl_ms: BTreeMap<Route, u64>,
        max_entry_bytes: usize,
        operation_timeout_ms: u64,
    },
}
impl Config {
    pub fn validate(&self) -> Result<(), AppError> {
        let valid = match self {
            Self::Disabled => true,
            Self::Memory {
                ttl_ms,
                route_ttl_ms,
                max_entries,
                max_bytes,
                max_entry_bytes,
            } => {
                (1..=300_000).contains(ttl_ms)
                    && route_ttl_ms.values().all(|ttl| *ttl <= 300_000)
                    && (1..=100_000).contains(max_entries)
                    && (1024..=1024 * 1024 * 1024).contains(max_bytes)
                    && (256..=8 * 1024 * 1024).contains(max_entry_bytes)
                    && max_entry_bytes <= max_bytes
            }
            Self::Redis {
                url_env,
                namespace,
                ttl_ms,
                route_ttl_ms,
                max_entry_bytes,
                operation_timeout_ms,
            } => {
                !url_env.is_empty()
                    && !namespace.is_empty()
                    && namespace.len() <= 64
                    && namespace
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    && route_ttl_ms.values().all(|ttl| *ttl <= 300_000)
                    && (1..=300_000).contains(ttl_ms)
                    && (256..=8 * 1024 * 1024).contains(max_entry_bytes)
                    && (1..=2000).contains(operation_timeout_ms)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(AppError::Config("invalid response cache configuration"))
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Entry {
    expires_ms: u64,
    value: Value,
}
#[derive(Default)]
struct Memory {
    entries: BTreeMap<String, Vec<u8>>,
    bytes: usize,
}
pub struct Cache {
    config: Config,
    fills: Vec<std::sync::Arc<tokio::sync::Mutex<()>>>,
    memory: Mutex<Memory>,
    redis: Option<redis::aio::ConnectionManager>,
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
impl Cache {
    pub fn new(config: Config) -> Result<Self, AppError> {
        config.validate()?;
        let redis = if let Config::Redis {
            url_env,
            operation_timeout_ms,
            ..
        } = &config
        {
            let secret = crate::config::secret(url_env)?;
            let url =
                url::Url::parse(&secret).map_err(|_| AppError::Config("invalid Redis URL"))?;
            if !matches!(url.scheme(), "redis" | "rediss")
                || url.host_str().is_none()
                || url.fragment().is_some()
                || url.query().is_some()
            {
                return Err(AppError::Config(
                    "Redis URL must use redis or rediss without query/fragment",
                ));
            }
            let client =
                redis::Client::open(secret).map_err(|_| AppError::Config("invalid Redis URL"))?;
            let timeout = Duration::from_millis(*operation_timeout_ms);
            let policy = redis::aio::ConnectionManagerConfig::new()
                .set_number_of_retries(0)
                .set_connection_timeout(Some(timeout))
                .set_response_timeout(Some(timeout));
            Some(
                redis::aio::ConnectionManager::new_lazy_with_config(client, policy)
                    .map_err(|_| AppError::Config("invalid Redis client configuration"))?,
            )
        } else {
            None
        };
        Ok(Self {
            config,
            fills: (0..64)
                .map(|_| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .collect(),
            memory: Mutex::new(Memory::default()),
            redis,
        })
    }
    pub fn enabled(&self) -> bool {
        !matches!(self.config, Config::Disabled)
    }
    pub fn ttl(&self, rpc: &str) -> Option<u64> {
        let route = Route::from_rpc(rpc)?;
        let ttl = match &self.config {
            Config::Disabled => return None,
            Config::Memory {
                ttl_ms,
                route_ttl_ms,
                ..
            }
            | Config::Redis {
                ttl_ms,
                route_ttl_ms,
                ..
            } => *route_ttl_ms.get(&route).unwrap_or(ttl_ms),
        };
        (ttl > 0).then_some(ttl)
    }
    /// Bounded striped locks coalesce fills within one region/process.
    pub async fn fill_guard(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let hash = key.bytes().fold(0usize, |h, byte| {
            h.wrapping_mul(31).wrapping_add(byte as usize)
        });
        self.fills[hash % self.fills.len()]
            .clone()
            .lock_owned()
            .await
    }
    pub async fn put_route(&self, rpc: &str, key: String, value: &Value) {
        if let Some(ttl) = self.ttl(rpc) {
            self.put_with_ttl(key, value, Some(ttl)).await;
        }
    }
    pub async fn get(&self, key: &str) -> Option<Value> {
        let bytes = match &self.config {
            Config::Disabled => return None,
            Config::Memory { .. } => {
                let mut memory = self.memory.lock().ok()?;
                let bytes = memory.entries.get(key)?.clone();
                let entry: Entry = serde_json::from_slice(&bytes).ok()?;
                if entry.expires_ms <= now_ms() {
                    memory.entries.remove(key);
                    memory.bytes -= bytes.len();
                    return None;
                }
                return Some(entry.value);
            }
            Config::Redis {
                namespace,
                max_entry_bytes,
                operation_timeout_ms,
                ..
            } => {
                let mut connection = self.redis.as_ref()?.clone();
                // Bounded GETRANGE prevents an oversized externally-written entry from being fetched.
                let mut cmd = redis::cmd("GETRANGE");
                cmd.arg(format!("{namespace}:{key}"))
                    .arg(0)
                    .arg(*max_entry_bytes);
                let bytes = tokio::time::timeout(
                    Duration::from_millis(*operation_timeout_ms),
                    cmd.query_async::<Vec<u8>>(&mut connection),
                )
                .await
                .ok()?
                .ok()?;
                if bytes.len() > *max_entry_bytes {
                    return None;
                }
                bytes
            }
        };
        let entry: Entry = serde_json::from_slice(&bytes).ok()?;
        (entry.expires_ms > now_ms()).then_some(entry.value)
    }
    pub async fn put(&self, key: String, value: &Value) {
        self.put_with_ttl(key, value, None).await;
    }
    async fn put_with_ttl(&self, key: String, value: &Value, override_ttl: Option<u64>) {
        let (ttl, limit) = match &self.config {
            Config::Disabled => return,
            Config::Memory {
                ttl_ms,
                max_entry_bytes,
                ..
            }
            | Config::Redis {
                ttl_ms,
                max_entry_bytes,
                ..
            } => (*ttl_ms, *max_entry_bytes),
        };
        let ttl = override_ttl.unwrap_or(ttl);
        let Ok(bytes) = serde_json::to_vec(&Entry {
            expires_ms: now_ms().saturating_add(ttl),
            value: value.clone(),
        }) else {
            return;
        };
        if bytes.len() > limit {
            return;
        }
        match &self.config {
            Config::Memory {
                max_entries,
                max_bytes,
                ..
            } => {
                let Ok(mut memory) = self.memory.lock() else {
                    return;
                };
                if let Some(old) = memory.entries.remove(&key) {
                    memory.bytes -= old.len();
                }
                while memory.entries.len() >= *max_entries
                    || memory.bytes + bytes.len() > *max_bytes
                {
                    let Some((_, removed)) = memory.entries.pop_first() else {
                        break;
                    };
                    memory.bytes -= removed.len();
                }
                memory.bytes += bytes.len();
                memory.entries.insert(key, bytes);
            }
            Config::Redis {
                namespace,
                operation_timeout_ms,
                ..
            } => {
                let Some(mut connection) = self.redis.clone() else {
                    return;
                };
                let mut cmd = redis::cmd("SET");
                cmd.arg(format!("{namespace}:{key}"))
                    .arg(bytes)
                    .arg("PX")
                    .arg(ttl);
                let _ = tokio::time::timeout(
                    Duration::from_millis(*operation_timeout_ms),
                    cmd.query_async::<()>(&mut connection),
                )
                .await;
            }
            Config::Disabled => {}
        }
    }
}
