//! Bounded owner-to-consumer wakeup delivery. Acceptance is not synchronization success.
use crate::{master_registry::Scope, master_sync::UpdateHint};
use reqwest::{header, StatusCode};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Master notification destination or hint")]
    Config,
    #[error("Master notification delivery failed")]
    Delivery,
}

/// One consumer's delivery state. A restart may resend the current hint safely.
/// Failed requests do not advance the acknowledgement, so the caller can retry.
pub struct Target {
    http: reqwest::Client,
    url: reqwest::Url,
    authorization: header::HeaderValue,
    scope: Scope,
    accepted: Option<String>,
}
impl Target {
    pub fn new(
        origin: &str,
        token: &str,
        scope: Scope,
        regional_paths: bool,
        allow_http: bool,
        timeout_ms: u64,
    ) -> Result<Self, Error> {
        if scope.region != crate::region::Region::Jp || !(100..=30_000).contains(&timeout_ms) {
            return Err(Error::Config);
        }
        // Use the same strict origin/token rules as the peer transport.
        crate::peer_transport::Client::new(
            origin,
            token,
            scope.region,
            regional_paths,
            allow_http,
            crate::peer_transport::Policy {
                connect_timeout_ms: timeout_ms.min(5_000),
                request_timeout_ms: timeout_ms,
                ..Default::default()
            },
        )
        .map_err(|_| Error::Config)?;
        let mut url = reqwest::Url::parse(origin).map_err(|_| Error::Config)?;
        url.set_path(&if regional_paths {
            format!("/internal/v1/{}/master-data/sync", scope.region.name())
        } else {
            "/internal/v1/master-data/sync".into()
        });
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Config)?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_millis(timeout_ms.min(5_000)))
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            http,
            url,
            authorization,
            scope,
            accepted: None,
        })
    }
    /// Returns false when this content was already acknowledged. Callers must
    /// obtain the hash from committed CURRENT, never an uncommitted staging tree.
    pub async fn deliver(&mut self, content_sha256: &str) -> Result<bool, Error> {
        if content_sha256.len() != 64 || !content_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Config);
        }
        if self.accepted.as_deref() == Some(content_sha256) {
            return Ok(false);
        }
        let hint = UpdateHint {
            scope: self.scope.clone(),
            content_sha256: content_sha256.into(),
        };
        let mut response = self
            .http
            .post(self.url.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&hint).map_err(|_| Error::Config)?)
            .send()
            .await
            .map_err(|_| Error::Delivery)?;
        if response.status() != StatusCode::ACCEPTED
            || response.content_length().is_some_and(|n| n > 1024)
        {
            return Err(Error::Delivery);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Delivery)? {
            if bytes.len().saturating_add(chunk.len()) > 1024 {
                return Err(Error::Delivery);
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            status: String,
        }
        let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| Error::Delivery)?;
        if reply.status != "accepted" {
            return Err(Error::Delivery);
        }
        self.accepted = Some(content_sha256.into());
        Ok(true)
    }
}
