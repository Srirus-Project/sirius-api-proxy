//! Global (HK/EN/KR) OneSDK guest requests: `tourist.login` creates a guest identity and
//! `cache.login` revalidates one. Signing and form encoding follow the Global 1.0.1 Android SDK
//! (Google channel). Requests go only to the three official HTTPS origins, never follow
//! redirects, are never retried and read at most 1 MiB. No value handled here implements Debug,
//! and nothing here logs request or response contents.
use md5::{Digest, Md5};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// The only SDK origins requests may target.
pub const SDK_ORIGINS: [&str; 3] = [
    "https://l11-sdk-login-intl.biligame.net",
    "https://l12-sdk-login-intl.biligame.net",
    "https://l13-sdk-login-intl.biligame.net",
];
pub const DEFAULT_SDK_ORIGIN: &str = SDK_ORIGINS[0];
pub const TOURIST_LOGIN: &str = "/gapi/client/tourist.login";
pub const CACHE_LOGIN: &str = "/gapi/client/cache.login";
/// The SDK asks for an interactive CAPTCHA; automation must stop.
pub const CAPTCHA_CODE: i64 = 200007;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 BSGameSDK";
const ONE_SDK_VERSION: &str = "1.25.0";
/// Fixed parameters of the Global 1.0.1 Google-channel APK (resources and SDK constants).
/// The HTTP `channel_id` is always 100; it is not the OneSDK global channel 2001.
const FIXED_PARAMETERS: [(&str, &str); 11] = [
    ("game_id", "17703"),
    ("server_id", "16841"),
    ("merchant_id", "1045"),
    ("app_ver", "1.0.1"),
    ("sdk_ver", "4.2.12"),
    ("channel_id", "100"),
    ("platform", "google"),
    ("platform_type", "3"),
    ("sdk_log_type", "1"),
    ("ad_ext", ""),
    ("web_code", "6"),
];

/// Whether `origin` is an allowed SDK origin. Unit tests may also use a loopback HTTP mock.
pub fn origin_allowed(origin: &str) -> bool {
    SDK_ORIGINS.contains(&origin) || (cfg!(test) && origin.starts_with("http://127.0.0.1:"))
}

/// Device context of one SDK identity, collected from one real Android device or emulator and
/// kept stable for the identity's lifetime. The SDK fields are the Android SDK common parameters;
/// the `unity_*` fields are Unity `SystemInfo.deviceModel`, `operatingSystem` and
/// `deviceUniqueIdentifier` of the same device, sent in the game-layer PlayerLogin.
#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub udid: String,
    pub model: String,
    pub pf_ver: String,
    pub dp: String,
    pub net: String,
    #[serde(default = "default_operators")]
    pub operators: String,
    #[serde(default)]
    pub adid: String,
    pub lang: String,
    pub time_zone: String,
    #[serde(rename = "isRoot")]
    pub is_root: String,
    pub unity_device_model: String,
    pub unity_operating_system: String,
    pub unity_device_id: String,
}
fn default_operators() -> String {
    // CommonUtils.getOperator returns "5" unconditionally in this SDK version.
    "5".into()
}
fn plain(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(char::is_control)
}
impl Device {
    /// Required values are present, bounded and free of control characters. `adid` may be empty
    /// (the client sends an empty value until an advertising ID is available).
    pub fn validate(&self) -> Result<(), &'static str> {
        let required = [
            &self.udid,
            &self.model,
            &self.pf_ver,
            &self.dp,
            &self.net,
            &self.operators,
            &self.lang,
            &self.time_zone,
            &self.is_root,
            &self.unity_device_model,
            &self.unity_operating_system,
            &self.unity_device_id,
        ];
        if required
            .iter()
            .any(|v| v.trim().is_empty() || !plain(v, 256))
            || !plain(&self.adid, 256)
        {
            return Err("device fields must be nonblank, at most 256 characters and without control characters");
        }
        Ok(())
    }
    fn parameters(&self) -> [(&'static str, &str); 10] {
        [
            ("udid", &self.udid),
            ("model", &self.model),
            ("pf_ver", &self.pf_ver),
            ("dp", &self.dp),
            ("net", &self.net),
            ("operators", &self.operators),
            ("adid", &self.adid),
            ("lang", &self.lang),
            ("time_zone", &self.time_zone),
            ("isRoot", &self.is_root),
        ]
    }
}

/// An SDK identity: `uid` (sent as `x-player-bid` and PlayerLogin `sdkUid`), `access_key`
/// (PlayerLogin `sdkAccessToken`) and the optional OpenID `id_token`.
#[derive(Clone)]
pub struct SdkAccount {
    pub uid: String,
    pub access_key: String,
    pub id_token: String,
    pub mid: Option<String>,
}

/// SDK failure classes. None carries response text, identities or tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdkError {
    /// Invalid origin, key or client configuration.
    Config,
    /// Code 200007: an interactive CAPTCHA is required. Never automated.
    Captcha,
    /// Any other nonzero SDK code.
    Refused(i64),
    /// Missing identity, or `cache.login` answered for a different uid.
    Identity,
    /// Connection failure or timeout; the outcome is unknown.
    Transport,
    /// Non-200 status, redirect, oversized or malformed response.
    Protocol,
}
impl SdkError {
    /// Stable application-style code for status and logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::Config => "SDK_CONFIG",
            Self::Captcha => "SDK_CAPTCHA",
            Self::Refused(_) => "SDK_REFUSED",
            Self::Identity => "SDK_IDENTITY",
            Self::Transport => "SDK_TRANSPORT",
            Self::Protocol => "SDK_PROTOCOL",
        }
    }
    /// Transient failures cool an account down; all others need an operator.
    pub fn transient(self) -> bool {
        matches!(self, Self::Transport | Self::Protocol)
    }
}
impl std::fmt::Display for SdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config => write!(f, "SDK configuration is invalid"),
            Self::Captcha => write!(
                f,
                "SDK requires CAPTCHA; finish verification in the official client"
            ),
            Self::Refused(code) => write!(f, "SDK refused the request: code={code}"),
            Self::Identity => write!(f, "SDK response identity is missing or changed"),
            Self::Transport => write!(
                f,
                "SDK transport failed; the outcome is unknown and the request is not repeated"
            ),
            Self::Protocol => write!(f, "SDK response is invalid"),
        }
    }
}

/// OneSDK signature: parameter names in Java `String` order (UTF-16 code units), their decoded
/// values concatenated (excluding `item_name`/`item_desc`, case-insensitively), the app key
/// appended, then lower-case hex MD5 of the UTF-8 bytes. `sign` itself is never included.
pub fn sign(parameters: &[(String, String)], app_key: &str) -> String {
    let mut names = parameters.iter().collect::<Vec<_>>();
    names.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let mut joined = String::new();
    for (name, value) in names {
        if !matches!(name.to_lowercase().as_str(), "item_name" | "item_desc") {
            joined.push_str(value);
        }
    }
    joined.push_str(app_key);
    Md5::digest(joined.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `application/x-www-form-urlencoded` exactly like Python `urlencode` (`quote_plus`): space is
/// `+`, `A-Za-z0-9_.-~` are literal, every other UTF-8 byte is `%XX` upper-case.
pub fn form_encode(parameters: &[(String, String)]) -> String {
    fn push(out: &mut String, value: &str) {
        for byte in value.bytes() {
            match byte {
                b' ' => out.push('+'),
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (name, value)) in parameters.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        push(&mut out, name);
        out.push('=');
        push(&mut out, value);
    }
    out
}

/// Signed parameters for `tourist.login` (`cached` = None) or `cache.login`.
pub fn prepare(
    device: &Device,
    app_key: &str,
    timestamp_ms: u128,
    cached: Option<&SdkAccount>,
) -> Vec<(String, String)> {
    let mut parameters = FIXED_PARAMETERS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .chain(
            device
                .parameters()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        )
        .collect::<Vec<_>>();
    parameters.push(("timestamp".into(), timestamp_ms.to_string()));
    if let Some(account) = cached {
        parameters.push(("access_key".into(), account.access_key.clone()));
        // NetworkUtil adds uid and mid when the SDK User is loaded.
        parameters.push(("uid".into(), account.uid.clone()));
        if let Some(mid) = &account.mid {
            parameters.push(("mid".into(), mid.clone()));
        }
    }
    let signature = sign(&parameters, app_key);
    parameters.push(("sign".into(), signature));
    parameters
}

fn identity_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.is_u64() || n.is_i64() => Some(n.to_string()),
        _ => None,
    }
}
fn header_safe(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && value.parse::<hyper::header::HeaderValue>().is_ok()
}
/// Interpret an SDK envelope. `previous` is the identity a `cache.login` revalidated.
pub fn interpret(body: &Value, previous: Option<&SdkAccount>) -> Result<SdkAccount, SdkError> {
    let code = body
        .get("code")
        .and_then(Value::as_i64)
        .filter(|_| body.get("code").is_some_and(Value::is_number))
        .ok_or(SdkError::Protocol)?;
    if code == CAPTCHA_CODE {
        return Err(SdkError::Captcha);
    }
    if code != 0 {
        return Err(SdkError::Refused(code));
    }
    let data = body
        .get("data")
        .and_then(Value::as_object)
        .ok_or(SdkError::Identity)?;
    let uid = identity_text(data.get("uid")).ok_or(SdkError::Identity)?;
    let id_token = data
        .get("id_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mid = identity_text(data.get("mid"));
    let account = match previous {
        Some(previous) => {
            if uid != previous.uid {
                return Err(SdkError::Identity);
            }
            // CacheLoginActivity restores the original access key on the returned User.
            SdkAccount {
                uid,
                access_key: previous.access_key.clone(),
                id_token: id_token.unwrap_or_else(|| previous.id_token.clone()),
                mid: mid.or_else(|| previous.mid.clone()),
            }
        }
        None => SdkAccount {
            uid,
            access_key: data
                .get("access_key")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(SdkError::Identity)?,
            id_token: id_token.unwrap_or_default(),
            mid,
        },
    };
    if !header_safe(&account.uid)
        || !header_safe(&account.access_key)
        || !(account.id_token.is_empty() || header_safe(&account.id_token))
    {
        return Err(SdkError::Identity);
    }
    Ok(account)
}

/// HTTPS client for one allowed SDK origin.
pub struct SdkClient {
    http: reqwest::Client,
    origin: String,
    app_key: String,
}
impl SdkClient {
    pub fn new(
        origin: &str,
        app_key: String,
        timeout: Duration,
        proxy: Option<reqwest::Proxy>,
    ) -> Result<Self, SdkError> {
        if !origin_allowed(origin) || app_key.trim().is_empty() {
            return Err(SdkError::Config);
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .https_only(!cfg!(test))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .timeout(timeout);
        if let Some(proxy) = proxy {
            builder = builder.proxy(proxy);
        }
        Ok(Self {
            http: builder.build().map_err(|_| SdkError::Config)?,
            origin: origin.to_owned(),
            app_key,
        })
    }
    fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }
    /// Create a new guest identity. Sent exactly once; never retried.
    pub async fn tourist_login(&self, device: &Device) -> Result<SdkAccount, SdkError> {
        device.validate().map_err(|_| SdkError::Config)?;
        let parameters = prepare(device, &self.app_key, Self::now_ms(), None);
        let body = self.post(TOURIST_LOGIN, form_encode(&parameters)).await?;
        interpret(&body, None)
    }
    /// Revalidate an existing identity; the uid must not change and the access key is kept.
    pub async fn cache_login(
        &self,
        device: &Device,
        account: &SdkAccount,
    ) -> Result<SdkAccount, SdkError> {
        device.validate().map_err(|_| SdkError::Config)?;
        let parameters = prepare(device, &self.app_key, Self::now_ms(), Some(account));
        let body = self.post(CACHE_LOGIN, form_encode(&parameters)).await?;
        interpret(&body, Some(account))
    }
    async fn post(&self, path: &str, body: String) -> Result<Value, SdkError> {
        let mut response = self
            .http
            .post(format!("{}{path}", self.origin))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("api-version", "1")
            .header("one-sdk-ver", ONE_SDK_VERSION)
            .body(body)
            .send()
            .await
            .map_err(|_| SdkError::Transport)?;
        if response.status() != reqwest::StatusCode::OK
            || response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(SdkError::Protocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| SdkError::Transport)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(SdkError::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
            .ok_or(SdkError::Protocol)
    }
}
