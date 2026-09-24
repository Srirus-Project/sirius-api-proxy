use crate::error::AppError;
use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Release {
    version: String,
    #[serde(rename = "iOS")]
    ios: String,
    #[serde(rename = "minClientVersion")]
    minimum: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ResourceSnapshot {
    pub schema_version: u8,
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
}
pub(crate) fn select(raw: &str, client: &str) -> Result<(String, String), AppError> {
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
    for part in [&chosen.version, &chosen.ios] {
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
    Ok((chosen.version, chosen.ios))
}
