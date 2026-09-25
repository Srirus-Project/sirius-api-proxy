//! Ordered, bounded node selection for verified public reads only.
use crate::{
    client::{GameClient, Observation},
    config::secret,
    error::AppError,
    peer::{self, Failure, Operation, Outcome},
    peer_transport,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub local_priority: Option<i32>,
    pub targets: Vec<TargetConfig>,
    pub timeout_ms: u64,
    pub max_inflight: usize,
    pub failure_threshold: u32,
    pub cooldown_ms: u64,
    pub transport: peer_transport::Policy,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            local_priority: Some(0),
            targets: Vec::new(),
            timeout_ms: 20_000,
            max_inflight: 64,
            failure_threshold: 3,
            cooldown_ms: 30_000,
            transport: Default::default(),
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub name: String,
    pub origin: String,
    pub token_env: String,
    #[serde(default = "priority")]
    pub priority: i32,
    #[serde(default)]
    pub regional_paths: bool,
    #[serde(default)]
    pub allow_http: bool,
}
fn priority() -> i32 {
    10
}
impl Config {
    pub fn validate(&self, region: crate::region::Region) -> Result<(), AppError> {
        if self.targets.len() > 16
            || (self.targets.is_empty() && self.local_priority.is_none())
            || !(100..=300_000).contains(&self.timeout_ms)
            || !(1..=4096).contains(&self.max_inflight)
            || !(1..=100).contains(&self.failure_threshold)
            || !(100..=300_000).contains(&self.cooldown_ms)
        {
            return Err(AppError::Config("invalid node routing bounds"));
        }
        self.transport
            .validate()
            .map_err(|_| AppError::Config("invalid peer transport policy"))?;
        let mut names = std::collections::BTreeSet::new();
        let mut destinations = std::collections::BTreeSet::new();
        for target in &self.targets {
            if target.name.is_empty()
                || target.name.len() > 64
                || target.name == "local"
                || !target
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                || !names.insert(&target.name)
                || target.token_env.is_empty()
                || target.token_env.len() > 256
                || !target
                    .token_env
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(AppError::Config(
                    "invalid or duplicate node name or credential reference",
                ));
            }
            peer_transport::Client::new(
                &target.origin,
                "validation",
                region,
                target.regional_paths,
                target.allow_http,
                self.transport.clone(),
            )
            .map_err(|_| AppError::Config("invalid peer destination"))?;
            let url = url::Url::parse(&target.origin)
                .map_err(|_| AppError::Config("invalid peer destination"))?;
            if !destinations.insert((url.origin().ascii_serialization(), target.regional_paths)) {
                return Err(AppError::Config("duplicate peer destination"));
            }
        }
        Ok(())
    }
}
pub struct Execution {
    pub result: Result<Value, AppError>,
    pub observation: Observation,
}
impl Execution {
    fn error(error: AppError) -> Self {
        Self {
            result: Err(error),
            observation: Observation::default(),
        }
    }
}
#[derive(Default)]
struct Health {
    failures: u32,
    open_until: Option<Instant>,
    probing: bool,
}
struct Target {
    name: String,
    priority: i32,
    remote: Option<peer_transport::Client>,
    health: Mutex<Health>,
}
struct Permit<'a> {
    target: &'a Target,
    probe: bool,
}
impl Target {
    fn admit(&self) -> Option<Permit<'_>> {
        let mut health = self.health.lock().ok()?;
        if let Some(until) = health.open_until {
            if until > Instant::now() || health.probing {
                return None;
            }
            health.probing = true;
        }
        Some(Permit {
            target: self,
            probe: health.open_until.is_some(),
        })
    }
}
impl Permit<'_> {
    fn finish(&self, failed: bool, config: &Config) {
        if let Ok(mut health) = self.target.health.lock() {
            if failed {
                health.failures = health.failures.saturating_add(1);
                if health.failures >= config.failure_threshold {
                    health.open_until =
                        Some(Instant::now() + Duration::from_millis(config.cooldown_ms));
                }
            } else {
                health.failures = 0;
                health.open_until = None;
            }
        }
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if self.probe {
            if let Ok(mut h) = self.target.health.lock() {
                h.probing = false;
            }
        }
    }
}
pub struct Router {
    config: Config,
    targets: Vec<Target>,
    inflight: tokio::sync::Semaphore,
}
impl Router {
    pub fn new(config: Config, region: crate::region::Region) -> Result<Self, AppError> {
        config.validate(region)?;
        let mut targets = Vec::new();
        if let Some(priority) = config.local_priority {
            targets.push(Target {
                name: "local".into(),
                priority,
                remote: None,
                health: Default::default(),
            });
        }
        for target in &config.targets {
            let remote = peer_transport::Client::new(
                &target.origin,
                &secret(&target.token_env)?,
                region,
                target.regional_paths,
                target.allow_http,
                config.transport.clone(),
            )
            .map_err(|_| AppError::Config("invalid peer transport credential"))?;
            targets.push(Target {
                name: target.name.clone(),
                priority: target.priority,
                remote: Some(remote),
                health: Default::default(),
            });
        }
        // Stable sort keeps local first on ties, followed by configured remote order.
        targets.sort_by_key(|target| target.priority);
        let inflight = tokio::sync::Semaphore::new(config.max_inflight);
        Ok(Self {
            config,
            targets,
            inflight,
        })
    }
    pub fn status(&self) -> Value {
        json!({"enabled":true,"targets":self.targets.iter().map(|target| {
            let health=target.health.lock().unwrap_or_else(|e|e.into_inner());
            json!({"name":target.name,"priority":target.priority,"failures":health.failures,"probing":health.probing,
                "cooldown_remaining_ms":health.open_until.map_or(0,|until|until.saturating_duration_since(Instant::now()).as_millis() as u64)})
        }).collect::<Vec<_>>()})
    }
    pub async fn call(&self, client: &Arc<GameClient>, operation: Operation) -> Execution {
        let (route, input) = match operation.rpc() {
            Ok(v) => v,
            Err(e) => return Execution::error(e),
        };
        if !client.supported_routes().contains(&route) {
            return Execution::error(AppError::UnsupportedRegionOperation);
        }
        let identity = match client.peer_identity() {
            Ok(v) => v,
            Err(e) => return Execution::error(e),
        };
        let request = peer::Request {
            request_id: uuid::Uuid::new_v4().to_string(),
            identity,
            operation,
        };
        let deadline = Instant::now() + Duration::from_millis(self.config.timeout_ms);
        let _admission = match tokio::time::timeout_at(deadline, self.inflight.acquire()).await {
            Ok(Ok(v)) => v,
            _ => return Execution::error(AppError::Timeout),
        };
        let mut last = Execution::error(AppError::NodeUnavailable);
        for target in &self.targets {
            if Instant::now() >= deadline {
                return Execution::error(AppError::Timeout);
            }
            let Some(permit) = target.admit() else {
                continue;
            };
            let (execution, definitely_not_executed) = if let Some(remote) = &target.remote {
                match remote.call(&request, deadline).await {
                    Ok(reply) => {
                        let safe = matches!(
                            &reply.outcome,
                            Outcome::Failure {
                                kind: Failure::IdentityMismatch {}
                                    | Failure::UnsupportedOperation {}
                                    | Failure::UnavailableBeforeDispatch {}
                            }
                        );
                        let result = match reply.outcome {
                            Outcome::Success { data } => Ok(data),
                            Outcome::Failure { kind } => Err(failure_error(kind)),
                        };
                        (
                            Execution {
                                result,
                                observation: reply.observation,
                            },
                            safe,
                        )
                    }
                    Err(error) => {
                        let safe = error.definitely_not_sent();
                        let error = match error {
                            peer_transport::Error::Timeout | peer_transport::Error::NotSent => {
                                AppError::Timeout
                            }
                            peer_transport::Error::Protocol => AppError::Protocol,
                            _ => AppError::Transport,
                        };
                        (Execution::error(error), safe)
                    }
                }
            } else {
                let result = tokio::time::timeout_at(
                    deadline,
                    client.call_peer(route, input.clone(), &request.identity.protocol_sha256),
                )
                .await
                .unwrap_or(Err(AppError::Timeout));
                let safe = matches!(
                    &result,
                    Err(AppError::PeerIdentityMismatch
                        | AppError::PeerAccountUnavailable
                        | AppError::UnsupportedRegionOperation)
                );
                (
                    Execution {
                        result,
                        observation: client.observation().await,
                    },
                    safe,
                )
            };
            let target_fault = matches!(
                &execution.result,
                Err(AppError::Transport
                    | AppError::Proxy
                    | AppError::Timeout
                    | AppError::Protocol
                    | AppError::PeerIdentityMismatch
                    | AppError::PeerAccountUnavailable
                    | AppError::AccountUnavailable
                    | AppError::UnsupportedRegionOperation)
            );
            permit.finish(target_fault, &self.config);
            if !target_fault || (!definitely_not_executed && crate::client::authenticated(route)) {
                return execution;
            }
            last = execution;
        }
        last
    }
}
fn failure_error(failure: Failure) -> AppError {
    match failure {
        Failure::IdentityMismatch {} => AppError::PeerIdentityMismatch,
        Failure::UnsupportedOperation {} => AppError::UnsupportedRegionOperation,
        Failure::UnavailableBeforeDispatch {} => AppError::PeerAccountUnavailable,
        Failure::AccountUnavailable {} => AppError::AccountUnavailable,
        Failure::Timeout {} => AppError::Timeout,
        Failure::Transport {} => AppError::Transport,
        Failure::Protocol {} => AppError::Protocol,
        Failure::Game { grpc_status } => AppError::Grpc(grpc_status),
    }
}
