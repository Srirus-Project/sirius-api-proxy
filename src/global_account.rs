//! Global (HK/EN/KR) accounts: an SDK guest identity file per account and the in-memory game
//! session obtained from `PlayerLogin`. Identity values and sessions never implement Debug or
//! Serialize, are never logged and never appear in status, errors or cache keys.
use crate::{
    error::AppError,
    global_sdk::{Device, SdkAccount, SdkClient, SdkError},
    region::Region,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// OneSDK channel constants of the Global Google-channel APK (`one_global_*` resources),
/// verified live: they are the same for every server. The server is chosen by the API root.
pub const GLOBAL_CHANNEL_ID: u32 = 2001;
pub const BRAND_ID: u32 = 5;
pub const AREA_ID: u32 = 6;
pub const CLIENT_PACKAGE: &str = "com.bilibili.sirius";
pub const IDENTITY_SCHEMA: u32 = 1;
const MAX_IDENTITY_BYTES: u64 = 16 * 1024;
const DAY: Duration = Duration::from_secs(86_400);

/// `global_login` configuration section (HK/EN/KR only).
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoginConfig {
    /// One of the three official SDK origins.
    pub sdk_origin: String,
    /// Environment variable holding the APK `one_appkey` used to sign SDK requests.
    pub sdk_app_key_env: String,
    /// Deadline of one SDK request.
    pub sdk_timeout_ms: u64,
    /// Minimum time between two login attempts of one account.
    pub login_min_interval_seconds: u64,
    /// Login attempts allowed per account in any rolling 24 hours.
    pub max_logins_per_day: u32,
    /// Cooldown after an `AEGIS_*` (login queue / server full) signal.
    pub aegis_cooldown_seconds: u64,
    /// `CONCURRENT_DEVICE` signals per rolling 24 hours that disable the account.
    pub concurrent_device_limit: u32,
}
impl Default for LoginConfig {
    fn default() -> Self {
        Self {
            sdk_origin: crate::global_sdk::DEFAULT_SDK_ORIGIN.into(),
            sdk_app_key_env: "SIRIUS_GLOBAL_SDK_APP_KEY".into(),
            sdk_timeout_ms: 15_000,
            login_min_interval_seconds: 300,
            max_logins_per_day: 24,
            aegis_cooldown_seconds: 900,
            concurrent_device_limit: 3,
        }
    }
}
impl LoginConfig {
    pub fn validate(&self) -> Result<(), AppError> {
        if !crate::global_sdk::origin_allowed(&self.sdk_origin) {
            return Err(AppError::Config(
                "global_login.sdk_origin must be one of the official SDK HTTPS origins",
            ));
        }
        if self.sdk_app_key_env.is_empty()
            || !(1_000..=60_000).contains(&self.sdk_timeout_ms)
            || self.login_min_interval_seconds > 86_400
            || !(1..=100).contains(&self.max_logins_per_day)
            || !(60..=86_400).contains(&self.aegis_cooldown_seconds)
            || !(1..=100).contains(&self.concurrent_device_limit)
        {
            return Err(AppError::Config(
                "global_login values exceed supported bounds",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityFile {
    schema: u32,
    sdk: SdkSection,
    device: Device,
    #[serde(default)]
    players: BTreeMap<String, PlayerPin>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SdkSection {
    uid: Value,
    access_key: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    mid: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayerPin {
    expected_player_id: String,
}

/// A validated identity file.
pub struct Identity {
    pub sdk: SdkAccount,
    pub device: Device,
    /// Optional per-region pins: a PlayerLogin answering another player disables the account.
    pub players: BTreeMap<&'static str, String>,
}
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.is_u64() || n.is_i64() => Some(n.to_string()),
        _ => None,
    }
}
fn header_value(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 4096
        && value.parse::<hyper::header::HeaderValue>().is_ok()
}
impl Identity {
    pub fn parse(bytes: &[u8]) -> Result<Self, AppError> {
        let invalid = || AppError::Config("invalid Global identity file");
        let file: IdentityFile = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        if file.schema != IDENTITY_SCHEMA {
            return Err(invalid());
        }
        file.device.validate().map_err(|_| invalid())?;
        let sdk = SdkAccount {
            uid: text(&file.sdk.uid).ok_or_else(invalid)?,
            access_key: file.sdk.access_key,
            id_token: file.sdk.id_token,
            mid: file.sdk.mid.as_ref().and_then(text),
        };
        if !header_value(&sdk.uid)
            || !header_value(&sdk.access_key)
            || !(sdk.id_token.is_empty() || header_value(&sdk.id_token))
        {
            return Err(invalid());
        }
        let mut players = BTreeMap::new();
        for (name, pin) in file.players {
            let region = Region::from_config_name(&name)
                .filter(|r| r.family() == "global")
                .ok_or_else(invalid)?;
            if !header_value(&pin.expected_player_id) {
                return Err(invalid());
            }
            players.insert(region.name(), pin.expected_player_id);
        }
        Ok(Self {
            sdk,
            device: file.device,
            players,
        })
    }
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let bytes = crate::accounts::read_private_file(
            path,
            MAX_IDENTITY_BYTES,
            "Global identity file unavailable",
            "invalid Global identity file",
            "Global identity file must be private to its owner",
        )?;
        Self::parse(&bytes)
    }
}

/// The in-memory game session of one Global account in one region.
pub(crate) struct Session {
    pub player_id: String,
    pub credential: String,
    pub is_new_user: bool,
    pub cp_server_name: String,
}

/// Login lifecycle of one Global account. Guarded by a std mutex that is never held across
/// an await; logins themselves are serialized by the account's session lock.
#[derive(Default)]
pub(crate) struct LoginState {
    pub session: Option<Arc<Session>>,
    /// SDK identity revalidated by `cache.login` in this process (refreshed `id_token`).
    pub sdk: Option<SdkAccount>,
    /// Set by TOKEN_* signals: the next login starts with `cache.login` again.
    pub sdk_stale: bool,
    /// Login attempts (SDK + PlayerLogin) within the last 24 hours.
    pub attempts: VecDeque<Instant>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub last_error_code: Option<String>,
    /// Session invalidations since the last successful authenticated call.
    pub invalidations: u32,
    pub concurrent_device: VecDeque<Instant>,
}
impl LoginState {
    pub fn prune(&mut self, now: Instant) {
        while self
            .attempts
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= DAY)
        {
            self.attempts.pop_front();
        }
        while self
            .concurrent_device
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= DAY)
        {
            self.concurrent_device.pop_front();
        }
    }
    /// When the next login attempt is allowed, if not now.
    pub fn next_allowed(&mut self, now: Instant, config: &LoginConfig) -> Option<Instant> {
        self.prune(now);
        let interval = Duration::from_secs(config.login_min_interval_seconds);
        let by_interval = self
            .attempts
            .back()
            .map(|t| *t + interval)
            .filter(|t| *t > now);
        let by_cap = (self.attempts.len() >= config.max_logins_per_day as usize)
            .then(|| self.attempts.front().map(|t| *t + DAY))
            .flatten();
        by_interval.into_iter().chain(by_cap).max()
    }
}

/// One configured Global account: its identity, optional region pin and login state.
pub(crate) struct GlobalAccount {
    pub identity: Identity,
    pub expected_player_id: Option<String>,
    pub state: std::sync::Mutex<LoginState>,
}
impl GlobalAccount {
    pub fn new(identity: Identity, region: Region) -> Self {
        Self {
            expected_player_id: identity.players.get(region.name()).cloned(),
            identity,
            state: std::sync::Mutex::new(LoginState::default()),
        }
    }
    pub fn state(&self) -> std::sync::MutexGuard<'_, LoginState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// PlayerLogin request JSON (Protobuf JSON names of the verified Global descriptor). `platform`
/// (the client sends 0) and `uuid.adId` (empty) are default values and are not serialized.
pub(crate) fn login_request(sdk: &SdkAccount, device: &Device, client_version: &str) -> Value {
    let mut request = json!({
        "sdkUid": sdk.uid,
        "sdkAccessToken": sdk.access_key,
        "deviceModel": device.unity_device_model,
        "operatingSystem": device.unity_operating_system,
        "clientVersion": client_version,
        "uuid": {"identifier": device.unity_device_id},
        "clientPackage": CLIENT_PACKAGE,
        "globalChannelId": GLOBAL_CHANNEL_ID,
        "brandId": BRAND_ID,
        "areaId": AREA_ID,
    });
    if !sdk.id_token.is_empty() {
        request["idToken"] = json!(sdk.id_token);
    }
    request
}

/// Result of a login as parsed from the PlayerLogin response.
pub(crate) fn parse_login(value: &Value) -> Option<Session> {
    let credential = value.get("credential")?;
    let player_id = credential.get("id")?.as_str()?.to_owned();
    let secret = credential.get("credential")?.as_str()?.to_owned();
    if !header_value(&player_id) || !header_value(&secret) {
        return None;
    }
    Some(Session {
        player_id,
        credential: secret,
        is_new_user: value.get("isNewUser").and_then(Value::as_u64).unwrap_or(0) != 0,
        cp_server_name: value
            .get("cpServerName")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 128 && !s.chars().any(char::is_control))
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Options of `global-account bootstrap`.
pub struct BootstrapOptions {
    pub device_file: PathBuf,
    pub identity_file: PathBuf,
    pub sdk_origin: String,
    pub app_key_env: String,
    /// Without it the command is an offline plan and sends nothing.
    pub create_sdk_guest: bool,
}

fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn write_private(path: &Path, value: &Value) -> Result<(), &'static str> {
    use std::io::Write;
    let mut file = create_private(path).map_err(|_| "cannot create the private output file")?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| "cannot encode output")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "cannot write the private output file")
}
fn attempt_marker(identity: &Path) -> PathBuf {
    let mut name = identity.as_os_str().to_owned();
    name.push(".attempt.json");
    PathBuf::from(name)
}

/// One-shot SDK guest creation. Without `create_sdk_guest` it only validates the device file
/// and reports the plan. With it, it sends exactly one `tourist.login` and writes a schema-1
/// identity file (created exclusively, mode 0600, in a directory private to its owner). An
/// attempt marker is created before the request; an existing marker or identity file refuses,
/// so an uncertain outcome is never repeated automatically.
pub async fn bootstrap(options: &BootstrapOptions) -> Result<Value, &'static str> {
    let device_bytes = crate::accounts::read_private_file(
        &options.device_file,
        MAX_IDENTITY_BYTES,
        "device file unavailable",
        "invalid device file",
        "device file must be private to its owner",
    )
    .map_err(|_| "device file must be a private regular file of at most 16 KiB")?;
    let device: Device =
        serde_json::from_slice(&device_bytes).map_err(|_| "invalid device file")?;
    device.validate()?;
    if !crate::global_sdk::origin_allowed(&options.sdk_origin) {
        return Err("the SDK origin must be one of the official SDK HTTPS origins");
    }
    let marker = attempt_marker(&options.identity_file);
    if options.identity_file.exists() || marker.exists() {
        return Err(
            "the identity file or a previous attempt marker exists; inspect it, nothing was sent",
        );
    }
    let parent = options
        .identity_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = std::fs::metadata(parent).map_err(|_| "identity directory unavailable")?;
    if !metadata.is_dir() {
        return Err("identity directory unavailable");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("the identity directory must be private to its owner (mode 0700)");
        }
    }
    if !options.create_sdk_guest {
        return Ok(json!({
            "mode": "offline-plan",
            "network_requests": 0,
            "planned_requests": [{"operation": "tourist.login", "origin": options.sdk_origin}],
            "device_valid": true,
            "note": "pass --create-sdk-guest to create one SDK guest identity; it is not repeated",
        }));
    }
    let app_key = crate::config::secret(&options.app_key_env)
        .map_err(|_| "the SDK app key environment variable is missing or invalid")?;
    let client = SdkClient::new(&options.sdk_origin, app_key, Duration::from_secs(25), None)
        .map_err(|_| "invalid SDK client configuration")?;
    write_private(
        &marker,
        &json!({"operation": "tourist.login", "origin": options.sdk_origin, "at": Utc::now(), "retries": 0}),
    )?;
    let account = match client.tourist_login(&device).await {
        Ok(account) => account,
        Err(SdkError::Captcha) => {
            return Err("SDK requires CAPTCHA; finish verification in the official client")
        }
        Err(SdkError::Refused(_)) => return Err("SDK refused tourist.login"),
        Err(SdkError::Transport) => {
            return Err("SDK transport failed; the outcome is unknown; do not repeat automatically")
        }
        Err(_) => return Err("SDK response is invalid; the outcome is unknown"),
    };
    let mut sdk =
        json!({"uid": account.uid, "access_key": account.access_key, "id_token": account.id_token});
    if let Some(mid) = &account.mid {
        sdk["mid"] = json!(mid);
    }
    write_private(
        &options.identity_file,
        &json!({"schema": IDENTITY_SCHEMA, "sdk": sdk, "device": device, "players": {}}),
    )?;
    Ok(json!({
        "sdk_guest_created": true,
        "identity_file": options.identity_file,
        "game_login_verified": false,
        "next": "reference the identity file from a hk/en/kr account and run global-account verify",
    }))
}
