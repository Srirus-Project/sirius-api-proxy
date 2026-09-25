//! One-attempt peer HTTP transport. Node ordering and replay policy belong to the router.
use crate::{
    peer::{Failure, Identity, Outcome, Reply, Request},
    region::Region,
};
use reqwest::{header, StatusCode, Url};
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_response_bytes: usize,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 5_000,
            request_timeout_ms: 20_000,
            max_response_bytes: 16 * 1024 * 1024,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> Result<(), Error> {
        if !(100..=300_000).contains(&self.connect_timeout_ms)
            || !(100..=300_000).contains(&self.request_timeout_ms)
            || self.connect_timeout_ms > self.request_timeout_ms
            || !(1024..=128 * 1024 * 1024).contains(&self.max_response_bytes)
        {
            return Err(Error::Config);
        }
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid peer destination, policy or request")]
    Config,
    #[error("peer deadline expired before transmission")]
    NotSent,
    #[error("peer connection failed before transmission")]
    Connect,
    #[error("peer request timed out; execution is uncertain")]
    Timeout,
    #[error("peer transport failed; execution is uncertain")]
    Transport,
    #[error("peer returned HTTP {0}")]
    Status(u16),
    #[error("invalid peer response; execution is uncertain")]
    Protocol,
}
impl Error {
    /// Only these failures prove that this transport did not submit the query.
    pub fn definitely_not_sent(&self) -> bool {
        matches!(self, Self::Config | Self::NotSent | Self::Connect)
    }
}
pub struct Client {
    http: reqwest::Client,
    url: Url,
    region: Region,
    authorization: header::HeaderValue,
    policy: Policy,
}
impl Client {
    pub fn new(
        origin: &str,
        token: &str,
        region: Region,
        regional_paths: bool,
        allow_http: bool,
        policy: Policy,
    ) -> Result<Self, Error> {
        policy.validate()?;
        let mut url = Url::parse(origin).map_err(|_| Error::Config)?;
        if region == Region::Cn
            || origin.len() > 2048
            || origin.bytes().any(|b| b.is_ascii_whitespace())
            || origin.contains('\\')
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || !(url.scheme() == "https" || (allow_http && url.scheme() == "http"))
            || token.is_empty()
            || token.len() > 4096
            || !token.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(Error::Config);
        }
        url.set_path(&if regional_paths {
            format!("/internal/v1/{}/peer/query", region.name())
        } else {
            "/internal/v1/peer/query".into()
        });
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Config)?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_millis(policy.connect_timeout_ms))
            .timeout(Duration::from_millis(policy.request_timeout_ms))
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            http,
            url,
            region,
            authorization,
            policy,
        })
    }
    pub async fn call(
        &self,
        request: &Request,
        deadline: tokio::time::Instant,
    ) -> Result<Reply, Error> {
        validate_identity(&request.identity)?;
        let (route, _) = request.operation.rpc().map_err(|_| Error::Config)?;
        if request.identity.region != self.region
            || !crate::routes::for_family(self.region.family()).contains(&route)
            || uuid::Uuid::parse_str(&request.request_id)
                .map_or(true, |id| id.to_string() != request.request_id)
        {
            return Err(Error::Config);
        }
        let bytes = serde_json::to_vec(request).map_err(|_| Error::Config)?;
        if bytes.len() > 16 * 1024 {
            return Err(Error::Config);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::NotSent);
        }
        tokio::time::timeout_at(deadline, self.send(request, bytes))
            .await
            .map_err(|_| Error::Timeout)?
    }
    async fn send(&self, request: &Request, bytes: Vec<u8>) -> Result<Reply, Error> {
        let mut response = self
            .http
            .post(self.url.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() != StatusCode::OK {
            return Err(Error::Status(response.status().as_u16()));
        }
        if !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
            })
            || response
                .headers()
                .get(header::CONTENT_ENCODING)
                .is_some_and(|v| v != "identity")
            || response
                .content_length()
                .is_some_and(|n| n > self.policy.max_response_bytes as u64)
        {
            return Err(Error::Protocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > self.policy.max_response_bytes - bytes.len() {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
        if reply.request_id != request.request_id || reply.identity != request.identity {
            return Err(Error::Protocol);
        }
        match &reply.outcome {
            Outcome::Success { data } if !data.is_object() => return Err(Error::Protocol),
            Outcome::Failure {
                kind: Failure::Game { grpc_status },
            } if !(1..=16).contains(grpc_status) => return Err(Error::Protocol),
            _ => {}
        }
        Ok(reply)
    }
}
fn transport_error(error: reqwest::Error) -> Error {
    if error.is_connect() {
        Error::Connect
    } else if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Transport
    }
}
fn validate_identity(identity: &Identity) -> Result<(), Error> {
    if identity.contract_version != 1
        || identity.region == Region::Cn
        || identity.environment.is_empty()
        || identity.environment.len() > 256
        || !identity
            .environment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || semver::Version::parse(&identity.client_version).is_err()
        || identity.protocol_sha256.len() != 64
        || !identity
            .protocol_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Config);
    }
    Ok(())
}
