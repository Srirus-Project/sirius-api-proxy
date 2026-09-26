use crate::error::AppError;
use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Release {
    version: String,
    #[serde(rename = "iOS")]
    ios: Option<String>,
    #[serde(rename = "Android")]
    android: Option<String>,
    #[serde(rename = "minClientVersion")]
    minimum: Option<String>,
}
/// Resource snapshot served at `/internal/v1/resources/snapshot`.
///
/// JP snapshots keep schema 2 exactly (no layout fields), so existing consumers are unaffected.
/// Global (HK/EN/KR) snapshots use schema 3 and state the CDN layout explicitly: consumers must
/// recompute `catalog_url`/`bundle_base_url` from the layout and reject any difference.
#[derive(Clone, Debug, Serialize)]
pub struct ResourceSnapshot {
    pub schema_version: u8,
    pub region: crate::region::Region,
    pub environment: String,
    pub platform: &'static str,
    pub client_version: String,
    pub protocol_version: String,
    pub master_version: Option<String>,
    pub resource_version: String,
    pub platform_hash: String,
    pub effective_cdn_root: String,
    pub credential_ref: String,
    pub observed_at: DateTime<Utc>,
    pub source: &'static str,
    /// Schema 3: `jp` (`/asset/{version}/{platform}/{hash}/catalog_main.bin`) or `global`
    /// (`/asset/{platform}/catalog_{version}.bin`, bundles in `/asset/{platform}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_layout: Option<&'static str>,
    /// Schema 3: base (Japanese, no locale suffix) catalog URL under `effective_cdn_root`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_url: Option<String>,
    /// Schema 3: directory that remote bundle paths are resolved against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_base_url: Option<String>,
    /// Schema 3: `basic` (then `credential_ref` names the credential) or `none` (empty ref).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_authorization: Option<&'static str>,
}
/// Global client layout: (`catalog_{version}.bin`, its `.hash`, bundle directory) under
/// `{root}/asset/{platform}`.
pub(crate) fn global_catalog_urls(
    root: &str,
    platform: &str,
    version: &str,
) -> (String, String, String) {
    let base = format!("{root}/asset/{platform}");
    (
        format!("{base}/catalog_{version}.bin"),
        format!("{base}/catalog_{version}.hash"),
        base,
    )
}
/// Maximum accepted `.hash` body; the client's catalog hash is 32 hex digits.
const MAX_CATALOG_HASH: usize = 256;
/// Parses a catalog `.hash` body the way the client does (trimmed) and requires 32 hex digits.
pub(crate) fn catalog_hash(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim();
    (text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}
/// One bounded `.hash` GET per attempt: no redirects (client policy), exact 200, at most
/// [`MAX_CATALOG_HASH`] bytes. Errors never carry the URL, credential or response body.
pub(crate) async fn fetch_catalog_hash(
    http: &reqwest::Client,
    network: &crate::master_update::Network,
    url: &str,
    basic: Option<(&str, &str)>,
) -> Result<String, AppError> {
    use crate::master_update::UpdateError;
    let once = || async {
        let mut request = http.get(url);
        if let Some((username, password)) = basic {
            request = request.basic_auth(username, Some(password));
        }
        let mut response = request.send().await.map_err(|_| UpdateError::Download)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(UpdateError::Http(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_CATALOG_HASH as u64)
        {
            return Err(UpdateError::Master);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| UpdateError::Download)? {
            if bytes.len() + chunk.len() > MAX_CATALOG_HASH {
                return Err(UpdateError::Master);
            }
            bytes.extend_from_slice(&chunk);
        }
        catalog_hash(&bytes).ok_or(UpdateError::Master)
    };
    for attempt in 0..network.attempts {
        match once().await {
            Err(error) if network.retry(&error, attempt) => {
                tokio::time::sleep(network.delay(attempt)).await
            }
            Ok(hash) => return Ok(hash),
            Err(error) => {
                let status = match error {
                    UpdateError::Http(code) => Some(code),
                    _ => None,
                };
                tracing::warn!(
                    error_code = "catalog_hash_unavailable",
                    status,
                    "Global catalog hash request failed; resource snapshot stays unavailable"
                );
                return Err(AppError::SnapshotUnavailable);
            }
        }
    }
    Err(AppError::SnapshotUnavailable)
}
#[cfg(test)]
pub(crate) fn select(raw: &str, client: &str) -> Result<(String, String), AppError> {
    select_platform(raw, client, crate::region::Platform::Ios)
}
pub(crate) fn select_platform(
    raw: &str,
    client: &str,
    platform: crate::region::Platform,
) -> Result<(String, String), AppError> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|_| AppError::Protocol)?;
    let client = Version::parse(client).map_err(|_| AppError::Protocol)?;
    let chosen = if let Some(live) = v.get("live") {
        let releases: Vec<Release> =
            serde_json::from_value(live.clone()).map_err(|_| AppError::Protocol)?;
        let mut candidates = BTreeMap::new();
        for r in releases {
            let min = Version::parse(r.minimum.as_deref().ok_or(AppError::Protocol)?)
                .map_err(|_| AppError::Protocol)?;
            if candidates.insert(min, r).is_some() {
                return Err(AppError::Protocol);
            }
        }
        candidates
            .into_iter()
            .rfind(|(min, _)| min <= &client)
            .map(|(_, r)| r)
            .ok_or(AppError::SnapshotUnavailable)?
    } else {
        serde_json::from_value::<Release>(v).map_err(|_| AppError::Protocol)?
    };
    let hash = match platform {
        crate::region::Platform::Ios => chosen.ios,
        crate::region::Platform::Android => chosen.android,
    }
    .ok_or(AppError::SnapshotUnavailable)?;
    for part in [&chosen.version, &hash] {
        if part.is_empty()
            || part.len() > 256
            || matches!(part.as_str(), "." | "..")
            || !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err(AppError::Protocol);
        }
    }
    Ok((chosen.version, hash))
}
