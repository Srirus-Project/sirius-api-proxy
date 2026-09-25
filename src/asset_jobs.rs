//! Bounded transport for the Sirius asset updater job contract.
//! Dispatch scheduling and durable owner reconciliation are separate from HTTP acceptance.
use crate::region::Region;
use reqwest::{header, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid asset updater destination or request")]
    Config,
    #[error("asset updater transport failed")]
    Transport,
    #[error("asset updater returned HTTP {0}")]
    Status(u16),
    #[error("invalid asset updater response")]
    Protocol,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub region: Region,
    pub profile: String,
    pub operation: Operation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Update,
    Export,
    Verify,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}
#[derive(Debug, Deserialize)]
pub struct Job {
    pub id: String,
    pub request: Request,
    pub status: Status,
    pub idempotency_sha256: Option<String>,
    pub outcome: Option<Outcome>,
}
#[derive(Debug, Deserialize)]
pub struct Outcome {
    pub verification: Verification,
    pub export: Option<Export>,
    pub publication_id: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct Verification {
    pub region: Region,
    pub platform: String,
    pub environment: String,
    pub resource_version: String,
    pub platform_hash: String,
    pub catalog_sha256: String,
    pub full_catalog: bool,
    pub catalog_verified: bool,
}
#[derive(Debug, Deserialize)]
pub struct Export {
    pub full_export: bool,
    pub retained: bool,
    pub files: usize,
    pub bytes: u64,
}

pub struct Client {
    http: reqwest::Client,
    root: Url,
    authorization: header::HeaderValue,
    user_agent: Option<header::HeaderValue>,
}
impl Client {
    /// Credentials are scoped to this exact configured origin, with no redirects or ambient proxies.
    /// Plain HTTP requires explicit opt-in for private-network deployments.
    pub fn new(root: &str, token: &str, allow_http: bool, timeout_ms: u64) -> Result<Self, Error> {
        let root = Url::parse(root).map_err(|_| Error::Config)?;
        if root.host_str().is_none()
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
            || root.path() != "/"
            || !(root.scheme() == "https" || (allow_http && root.scheme() == "http"))
            || !(100..=300_000).contains(&timeout_ms)
            || token.is_empty()
            || token.len() > 4096
            || !token.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(Error::Config);
        }
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Config)?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(timeout_ms.min(10_000)))
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            http,
            root,
            authorization,
            user_agent: None,
        })
    }
    pub fn with_user_agent(mut self, value: Option<&str>) -> Result<Self, Error> {
        self.user_agent = value
            .map(|v| {
                if v.trim().is_empty()
                    || v.len() > 256
                    || !v.bytes().all(|b| (32..=126).contains(&b))
                {
                    return Err(Error::Config);
                }
                header::HeaderValue::from_str(v).map_err(|_| Error::Config)
            })
            .transpose()?;
        Ok(self)
    }
    pub async fn submit(&self, request: &Request, key: &str) -> Result<Job, Error> {
        validate_request(request)?;
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            return Err(Error::Config);
        }
        let url = self.root.join("api/v1/jobs").map_err(|_| Error::Config)?;
        let builder = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", key)
            .body(serde_json::to_vec(request).map_err(|_| Error::Config)?);
        let job = self.send(builder, StatusCode::ACCEPTED, request).await?;
        if job.idempotency_sha256.as_deref()
            != Some(format!("{:x}", Sha256::digest(key.as_bytes())).as_str())
        {
            return Err(Error::Protocol);
        }
        Ok(job)
    }
    pub async fn get(&self, id: &str, request: &Request) -> Result<Job, Error> {
        validate_request(request)?;
        let parsed = uuid::Uuid::parse_str(id).map_err(|_| Error::Config)?;
        if parsed.to_string() != id {
            return Err(Error::Config);
        }
        let url = self
            .root
            .join(&format!("api/v1/jobs/{id}"))
            .map_err(|_| Error::Config)?;
        let job = self
            .send(self.http.get(url), StatusCode::OK, request)
            .await?;
        if job.id != id {
            return Err(Error::Protocol);
        }
        Ok(job)
    }
    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
        expected: StatusCode,
        request: &Request,
    ) -> Result<Job, Error> {
        let builder = if let Some(agent) = &self.user_agent {
            builder.header(header::USER_AGENT, agent.clone())
        } else {
            builder
        };
        let mut response = builder
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status() != expected {
            return Err(Error::Status(response.status().as_u16()));
        }
        const LIMIT: usize = 64 * 1024;
        if response.content_length().is_some_and(|v| v > LIMIT as u64) {
            return Err(Error::Protocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if chunk.len() > LIMIT - bytes.len() {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        let job: Job = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
        if job.request != *request
            || uuid::Uuid::parse_str(&job.id).is_err()
            || (job.status != Status::Completed && job.outcome.is_some())
        {
            return Err(Error::Protocol);
        }
        if let Some(outcome) = &job.outcome {
            let v = &outcome.verification;
            if v.region != request.region
                || !v.catalog_verified
                || !matches!(v.platform.as_str(), "iOS" | "Android")
                || [&v.environment, &v.resource_version, &v.platform_hash]
                    .iter()
                    .any(|s| !component(s))
                || v.catalog_sha256.len() != 64
                || !v
                    .catalog_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || outcome
                    .publication_id
                    .as_ref()
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
                || (outcome.publication_id.is_some() && outcome.export.is_none())
            {
                return Err(Error::Protocol);
            }
        }
        Ok(job)
    }
}
fn component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !matches!(s, "." | "..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn validate_request(request: &Request) -> Result<(), Error> {
    if request.region == Region::Cn
        || request.profile.is_empty()
        || request.profile.len() > 64
        || !request
            .profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Err(Error::Config)
    } else {
        Ok(())
    }
}
