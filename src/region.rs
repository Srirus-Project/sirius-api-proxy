//! Region identity is independent of deployment environment and UI language.
//!
//! The Global Traditional-Chinese region is `hk`, the identifier the game itself uses (CDN
//! `/prod/hk_…`, `l12-prod-hk-…` endpoints, server list). Its pre-1.2.1 spelling is accepted
//! only as a deprecated alias when reading configuration and CLI arguments
//! ([`config_region`], [`Region::from_config_name`]) and when reading snapshot receipts and
//! Git state written by earlier builds ([`Region::from_recorded_name`]). It is never written,
//! served, routed or logged.
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    #[default]
    Jp,
    Hk,
    En,
    Kr,
    Cn,
}
/// Deprecated pre-1.2.1 spelling of [`Region::Hk`]; input-only (see the module docs).
const DEPRECATED_HK_ALIAS: &str = "tw";
static DEPRECATED_ALIAS_USED: AtomicBool = AtomicBool::new(false);
static DEPRECATED_ALIAS_WARNED: AtomicBool = AtomicBool::new(false);
/// Whether configuration or CLI input in this process used the deprecated `hk` alias.
pub fn deprecated_alias_used() -> bool {
    DEPRECATED_ALIAS_USED.load(Ordering::Relaxed)
}
/// Emit the deprecation warning at most once per process, after logging is initialized.
/// Returns whether this call logged it.
pub fn warn_deprecated_alias() -> bool {
    if !deprecated_alias_used() || DEPRECATED_ALIAS_WARNED.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        error_code = "deprecated_region_alias",
        region = "hk",
        "configuration uses the deprecated alias of the Traditional Chinese region; configure hk instead"
    );
    true
}
/// Whether `name` is the deprecated alias of [`Region::Hk`] (for configuration map keys).
pub(crate) fn is_deprecated_alias(name: &str) -> bool {
    name == DEPRECATED_HK_ALIAS
}
pub(crate) fn note_deprecated_alias() {
    DEPRECATED_ALIAS_USED.store(true, Ordering::Relaxed);
}
/// `deserialize_with` for configuration fields: the canonical names plus the deprecated alias.
pub fn config_region<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Region, D::Error> {
    let name = String::deserialize(deserializer)?;
    Region::from_config_name(&name)
        .ok_or_else(|| serde::de::Error::unknown_variant(&name, &["jp", "hk", "en", "kr", "cn"]))
}
impl Region {
    /// Parse a configuration or CLI region name, accepting the deprecated alias of `hk`.
    pub fn from_config_name(name: &str) -> Option<Self> {
        if is_deprecated_alias(name) {
            note_deprecated_alias();
            return Some(Self::Hk);
        }
        Self::from_name(name)
    }
    /// Parse a region recorded by an earlier build (snapshot receipts, Git state). The
    /// deprecated alias is read as `hk`; it is never written back.
    pub(crate) fn from_recorded_name(name: &str) -> Option<Self> {
        if is_deprecated_alias(name) {
            return Some(Self::Hk);
        }
        Self::from_name(name)
    }
    /// Parse a canonical region name only.
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Jp, Self::Hk, Self::En, Self::Kr, Self::Cn]
            .into_iter()
            .find(|region| region.name() == name)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Jp => "jp",
            Self::Hk => "hk",
            Self::En => "en",
            Self::Kr => "kr",
            Self::Cn => "cn",
        }
    }
    pub fn family(self) -> &'static str {
        match self {
            Self::Jp => "jp",
            Self::Cn => "cn",
            _ => "global",
        }
    }
    pub fn area_id(self) -> Option<&'static str> {
        match self {
            Self::Hk => Some("2"),
            Self::En => Some("3"),
            Self::Kr => Some("4"),
            _ => None,
        }
    }
    pub fn protocol_version(self) -> &'static str {
        match self {
            Self::Jp => "1.0.3",
            Self::Cn => "",
            _ => "1.0.1",
        }
    }
    pub fn matches_known_service(self, host: &str, path: &str) -> bool {
        let expected = if host.ends_with(".bang-dream-on.jp") {
            Some(Self::Jp)
        } else if host.ends_with(".gamerfusiontech.com") && host.contains("-prod-hk-") {
            Some(Self::Hk)
        } else if host.ends_with(".bilibiligame.net") && host.contains("-prod-va-") {
            Some(Self::En)
        } else if host.ends_with(".bilibiligame.net") && host.contains("-prod-kr-") {
            Some(Self::Kr)
        } else if host.ends_with(".bilibiligame.net") && host.contains("-prod-sg-patch-") {
            if path.starts_with("/prod/en_") {
                Some(Self::En)
            } else if path.starts_with("/prod/kr_") {
                Some(Self::Kr)
            } else {
                return false;
            }
        } else {
            None
        };
        expected.is_none_or(|region| region == self)
    }
    /// Regions whose Master pipeline (download, storage, registry, sync, notifications, Git
    /// and database publication) is verified. CN is reserved and never operational.
    pub fn master_supported(self) -> bool {
        !matches!(self, Self::Cn)
    }
    pub fn default_platform(self) -> Platform {
        if self == Self::Jp {
            Platform::Ios
        } else {
            Platform::Android
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Platform {
    #[serde(rename = "iOS")]
    Ios,
    Android,
}
impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ios => "iOS",
            Self::Android => "Android",
        }
    }
    pub fn header(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
        }
    }
}
