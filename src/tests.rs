use crate::{
    api,
    client::{GameClient, ANNOUNCEMENT, VERSION},
    config::Config,
    error::AppError,
    resources,
};
use bytes::Bytes;
use futures::stream;
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    header::HeaderMap,
    Request, Response,
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

fn config() -> Config {
    let key = format!("SIRIUS_TEST_CDN_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "fixture-cdn-secret");
    Config {
        region: crate::region::Region::Jp,
        platform: None,
        protocol_directory: crate::config::default_protocol_directory(),
        listen: Some("127.0.0.1:0".parse().unwrap()),
        environment: "release".into(),
        endpoint: "https://api.bang-dream-on.jp".into(),
        client_version: "1.0.3".into(),
        session_lock: true,
        upstream: Default::default(),
        api_token_env: "unused".into(),
        internal_token_env: "unused".into(),
        accounts: Vec::new(),
        account_pool: Default::default(),
        player_id_env: None,
        player_credential_env: None,
        master_directory: None,
        master_update: None,
        default_cdn_root: "https://static.bang-dream-on.jp".into(),
        cdn_credential_env: BTreeMap::from([("https://static.bang-dream-on.jp".into(), key)]),
    }
}
fn pool() -> DescriptorPool {
    DescriptorPool::decode(include_bytes!("../tests/fixtures/proxy-descriptors.pb").as_slice())
        .unwrap()
}
fn message(name: &str, value: Value) -> Vec<u8> {
    DynamicMessage::deserialize(pool().get_message_by_name(name).unwrap(), value)
        .unwrap()
        .encode_to_vec()
}
fn framed(bytes: Vec<u8>) -> Vec<u8> {
    let mut b = vec![0];
    b.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    b.extend(bytes);
    b
}
#[derive(Clone)]
struct Reply {
    bytes: Vec<u8>,
    headers: HeaderMap,
    trailers: HeaderMap,
    http_status: u16,
    delay: Duration,
    gate: Option<Arc<tokio::sync::Semaphore>>,
}
impl Reply {
    fn version() -> Self {
        Self {
            bytes: framed(message(
                "app.masterdata.VersionResponse",
                json!({"version":"master-fixture"}),
            )),
            headers: HeaderMap::new(),
            trailers: HeaderMap::from_iter([(
                "grpc-status".parse().unwrap(),
                "0".parse().unwrap(),
            )]),
            http_status: 200,
            delay: Duration::ZERO,
            gate: None,
        }
    }
    fn header(mut self, key: &'static str, value: &str) -> Self {
        self.headers.insert(key, value.parse().unwrap());
        self
    }
}
type ReceivedRequests = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;
struct Fixture {
    url: String,
    received: ReceivedRequests,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn fixture(replies: Vec<Reply>) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let seen = received.clone();
    let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            let replies = replies.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let seen = seen.clone();
                    let reply = replies.lock().unwrap().pop_front().expect("unexpected RPC");
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = body.collect().await.unwrap().to_bytes();
                        seen.lock().unwrap().push((
                            parts.uri.path().into(),
                            parts.headers,
                            body.to_vec(),
                        ));
                        if let Some(gate) = &reply.gate {
                            gate.acquire().await.unwrap().forget();
                        }
                        tokio::time::sleep(reply.delay).await;
                        // Split the unary message across HTTP DATA frames; gRPC framing is independent.
                        let split = reply.bytes.len().min(3);
                        let frames = vec![
                            Ok::<_, Infallible>(Frame::data(Bytes::copy_from_slice(
                                &reply.bytes[..split],
                            ))),
                            Ok(Frame::data(Bytes::copy_from_slice(&reply.bytes[split..]))),
                            Ok(Frame::trailers(reply.trailers)),
                        ];
                        let mut response = Response::builder()
                            .status(reply.http_status)
                            .header("content-type", "application/grpc+proto")
                            .body(StreamBody::new(stream::iter(frames)))
                            .unwrap();
                        response.headers_mut().extend(reply.headers);
                        Ok::<_, Infallible>(response)
                    }
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    Fixture {
        url,
        received,
        task,
    }
}
fn client(f: &Fixture, mut cfg: Config) -> Arc<GameClient> {
    cfg.endpoint = f.url.clone();
    GameClient::for_test(cfg)
}
async fn body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[test]
fn proxy_descriptor_and_int64_json_are_lossless() {
    let p = pool();
    assert_eq!(p.services().count(), 6);
    let m = DynamicMessage::deserialize(
        p.get_message_by_name("app.friend.FindByProfileIDRequest")
            .unwrap(),
        json!({"playerProfileId":"9223372036854775807"}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(m).unwrap()["playerProfileId"],
        "9223372036854775807"
    );
}
#[test]
fn version_selection_is_numeric_and_rejects_unsafe_or_inapplicable_paths() {
    let raw = r#"{"live":[{"minClientVersion":"1.0.10","version":"future","iOS":"h10"},{"minClientVersion":"1.0.2","version":"v2","iOS":"h2"},{"minClientVersion":"1.0.3","version":"v3","iOS":"h3"}]}"#;
    assert_eq!(
        resources::select(raw, "1.0.3").unwrap(),
        ("v3".into(), "h3".into())
    );
    assert!(resources::select(raw, "1.0.1").is_err());
    assert!(resources::select(r#"{"version":"../escape","iOS":"abc"}"#, "1.0.3").is_err());
    assert!(resources::select(r#"{"live":[],"version":"fallback","iOS":"abc"}"#, "1.0.3").is_err());
    assert_eq!(
        resources::select(r#"{"version":"v1","iOS":"abc"}"#, "1.0.3").unwrap(),
        ("v1".into(), "abc".into())
    );
}
#[test]
fn config_rejects_insecure_urls_credentials_paths_and_partial_accounts() {
    let c = config();
    assert!(c.validate().is_ok());
    for endpoint in [
        "http://example.com",
        "https://user:pass@example.com",
        "https://example.com/path",
        "https://example.com?x=1",
    ] {
        let mut c = c.clone();
        c.endpoint = endpoint.into();
        assert!(c.validate().is_err());
    }
    let mut c = c;
    c.player_id_env = Some("id".into());
    assert!(c.validate().is_err());
}
#[tokio::test]
async fn h2_unary_reads_trailers_and_sends_only_anonymous_headers() {
    let f = fixture(vec![Reply::version()]).await;
    let c = client(&f, config());
    assert_eq!(
        c.call(VERSION, json!({})).await.unwrap()["version"],
        "master-fixture"
    );
    let seen = f.received.lock().unwrap();
    assert_eq!(seen[0].0, VERSION);
    assert_eq!(seen[0].1["x-platform"], "ios");
    assert!(seen[0].1.contains_key("x-request-id"));
    assert!(!seen[0].1.contains_key("x-player-credential"));
    assert_eq!(seen[0].2, [0, 0, 0, 0, 0]);
}
#[tokio::test]
async fn trailer_only_maintenance_is_observed_without_secret_or_message_leak() {
    let mut reply = Reply::version().header("x-sirius-cred", "must-not-leak");
    reply.bytes.clear();
    reply.trailers.insert("grpc-status", "2".parse().unwrap());
    reply
        .trailers
        .insert("grpc-message", "secret%20must-not-leak".parse().unwrap());
    reply
        .trailers
        .insert("x-sirius-error-code", "UNDER_MAINTENANCE".parse().unwrap());
    let f = fixture(vec![reply]).await;
    let c = client(&f, config());
    let app = api::router(c, "api".into(), "internal".into());
    let r = app
        .oneshot(
            Request::get("/api/v1/system")
                .header("authorization", "Bearer api")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let value = body(r).await;
    assert_eq!(value["status"], "unavailable");
    assert_eq!(value["observation"]["maintenance"], true);
    assert!(!value.to_string().contains("must-not-leak"));
}
#[tokio::test]
async fn malformed_or_missing_grpc_status_is_not_success() {
    let mut missing = Reply::version();
    missing.trailers.clear();
    let mut malformed = Reply::version();
    malformed.bytes[4] = 255;
    let mut compressed = Reply::version();
    compressed.bytes[0] = 1;
    let mut http_error = Reply::version();
    http_error.http_status = 403;
    for reply in [missing, malformed, compressed, http_error] {
        let f = fixture(vec![reply]).await;
        let c = client(&f, config());
        assert!(matches!(
            c.call(VERSION, json!({})).await,
            Err(AppError::Protocol)
        ));
    }
}
#[tokio::test]
async fn profile_uses_public_id_and_only_profile_receives_account_headers() {
    let id = format!("SIRIUS_TEST_ID_{}", uuid::Uuid::new_v4().simple());
    let key = format!("{id}_KEY");
    std::env::set_var(&id, "fixture-player");
    std::env::set_var(&key, "fixture-account-secret");
    let mut cfg = config();
    cfg.player_id_env = Some(id);
    cfg.player_credential_env = Some(key);
    let mut reply = Reply::version();
    reply.bytes = framed(message(
        "app.friend.FindByProfileIDResponse",
        json!({"playerProfile":{"id":"player-x","profileId":"9007199254740993","name":"Example"}}),
    ));
    let f = fixture(vec![Reply::version(), reply]).await;
    let c = client(&f, cfg);
    let app = api::router(c, "api".into(), "internal".into());
    let response = app
        .oneshot(
            Request::get("/api/v1/players/by-profile-id/9007199254740993")
                .header("authorization", "Bearer api")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let v = body(response).await;
    assert_eq!(v["playerProfile"]["profileId"], "9007199254740993");
    let seen = f.received.lock().unwrap();
    assert!(!seen[0].1.contains_key("x-player-credential"));
    assert_eq!(seen[1].1["x-player-id"], "fixture-player");
    assert_eq!(seen[1].1["x-player-credential"], "fixture-account-secret");
    assert_eq!(seen[1].1["x-master-version"], "master-fixture");
    let request = DynamicMessage::decode(
        pool()
            .get_message_by_name("app.friend.FindByProfileIDRequest")
            .unwrap(),
        &seen[1].2[5..],
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(request).unwrap()["playerProfileId"],
        "9007199254740993"
    );
}
#[tokio::test]
async fn snapshot_is_version_pinned_and_invalid_rotation_marks_previous_stale() {
    let first = Reply::version()
        .header("x-asset-version", r#"{"version":"r1","iOS":"hash1"}"#)
        .header("x-sirius-cred", "fixture-cdn-secret");
    let second = Reply::version()
        .header("x-asset-version", r#"{"version":"r2","iOS":"hash2"}"#)
        .header("x-sirius-cred", "rotated-secret");
    let f = fixture(vec![first, second]).await;
    let c = client(&f, config());
    c.call(VERSION, json!({})).await.unwrap();
    let v = c.snapshot().await.unwrap();
    assert_eq!(v["stale"], false);
    assert_eq!(v["snapshot"]["resource_version"], "r1");
    assert_eq!(v["snapshot"]["master_version"], "master-fixture");
    assert!(!v.to_string().contains("fixture-cdn-secret"));
    c.call(VERSION, json!({})).await.unwrap();
    let v = c.snapshot().await.unwrap();
    assert_eq!(v["stale"], true);
    assert_eq!(v["snapshot"]["resource_version"], "r1");
}
#[tokio::test]
async fn unknown_cdn_never_produces_a_ready_snapshot() {
    let f = fixture(vec![Reply::version()
        .header("x-asset-version", r#"{"version":"r1","iOS":"h1"}"#)
        .header("x-sirius-env", "https://unconfigured.example")])
    .await;
    let c = client(&f, config());
    c.call(VERSION, json!({})).await.unwrap();
    assert!(c.snapshot().await.is_err());
}
#[tokio::test]
async fn http_auth_scope_validation_and_rpc_allowlist_block_before_upstream() {
    let f = fixture(vec![]).await;
    let c = client(&f, config());
    let app = api::router(c.clone(), "api".into(), "internal".into());
    for (path, token, expected) in [
        ("/health", "", 200),
        ("/api/v1/system", "", 401),
        ("/internal/v1/resources/snapshot", "api", 401),
        ("/api/cbt/system", "api", 404),
        ("/api/release/system", "api", 404),
        ("/internal/release/resource-snapshot", "internal", 404),
        ("/api/v2/system", "api", 404),
        ("/internal/v2/resources/snapshot", "internal", 404),
        ("/api/v1/profiles/1", "api", 404),
        ("/api/v1/music/1/ranking", "api", 404),
        ("/api/v1/players/by-profile-id/0", "api", 400),
        ("/api/v1/players/by-profile-id/1", "internal", 401),
        ("/api/v1/system", "internal", 401),
        ("/api/v1/announcements?tab=99", "api", 400),
        ("/api/v1/players/by-profile-id/1", "api", 503),
        ("/internal/v1/resources/snapshot", "internal", 503),
    ] {
        let request = Request::get(path)
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone()
                .oneshot(request)
                .await
                .unwrap()
                .status()
                .as_u16(),
            expected,
            "{path}"
        );
    }
    assert!(matches!(
        c.call("/app.player.PlayerService/Register", json!({}))
            .await,
        Err(AppError::InvalidRequest)
    ));
    assert!(f.received.lock().unwrap().is_empty());
}
#[tokio::test]
async fn announcement_serializes_nested_int64_and_enums() {
    let mut reply = Reply::version();
    reply.bytes = framed(message(
        "app.announcement.GetResponse",
        json!({"announcement":{"id":"9007199254740993","category":"MAINTENANCE","title":"Maintenance"}}),
    ));
    let f = fixture(vec![reply]).await;
    let c = client(&f, config());
    let v = c
        .call(ANNOUNCEMENT, json!({"id":"9007199254740993"}))
        .await
        .unwrap();
    assert_eq!(v["announcement"]["id"], "9007199254740993");
    assert_eq!(v["announcement"]["category"], "MAINTENANCE");
}

#[tokio::test]
async fn deadline_bounds_stalled_body_and_marks_snapshot_stale() {
    let mut reply = Reply::version();
    reply.delay = Duration::from_millis(300);
    let f = fixture(vec![reply]).await;
    let mut c = client(&f, config());
    GameClient::set_test_timeout(&mut c, Duration::from_millis(50));
    assert!(matches!(
        c.call(VERSION, json!({})).await,
        Err(AppError::Timeout)
    ));
    assert_eq!(c.observation().await.grpc_status, None);
}

#[tokio::test]
async fn oversized_response_is_rejected_before_protobuf_decode() {
    let mut reply = Reply::version();
    reply.bytes = vec![0; 8 * 1024 * 1024 + 1];
    let f = fixture(vec![reply]).await;
    let c = client(&f, config());
    assert!(matches!(
        c.call(VERSION, json!({})).await,
        Err(AppError::Protocol)
    ));
}

fn account_config() -> Config {
    let mut c = config();
    let id = format!("SIRIUS_RANK_ID_{}", uuid::Uuid::new_v4().simple());
    let credential = format!("{id}_SECRET");
    std::env::set_var(&id, "ranking-account");
    std::env::set_var(&credential, "ranking-secret");
    c.player_id_env = Some(id);
    c.player_credential_env = Some(credential);
    c
}
#[tokio::test]
async fn ranking_http_routes_encode_ids_and_strip_service_account_results() {
    for (path, response_name, response_json, request_name, request_json) in [
        (
            "/api/v1/events/9007199254740993/rankings?ranks=1,10,100",
            "app.event.GetRankingListResponse",
            json!({"ranking":[{"rank":10,"point":500}]}),
            "app.event.GetRankingListRequest",
            json!({"eventId":"9007199254740993","ranks":[1,10,100]}),
        ),
        (
            "/api/v1/events/1/players/player-42/deck",
            "app.event.GetDeckResponse",
            json!({}),
            "app.event.GetDeckRequest",
            json!({"eventId":"1","playerId":"player-42"}),
        ),
        (
            "/api/v1/songs/42/rankings",
            "app.livemusic.GetRankingResponse",
            json!({"myRank":123,"players":[{"score":456}]}),
            "app.livemusic.GetRankingRequest",
            json!({"musicId":"42"}),
        ),
        (
            "/api/v1/challenge-songs/43/rankings",
            "app.event.GetChallengeMusicRankingResponse",
            json!({"myRank":123,"myScore":456,"players":[{"score":789}]}),
            "app.event.GetChallengeMusicRankingRequest",
            json!({"challengeMusicId":"43"}),
        ),
    ] {
        let mut reply = Reply::version();
        reply.bytes = framed(message(response_name, response_json));
        let f = fixture(vec![Reply::version(), reply]).await;
        let app = api::router(
            client(&f, account_config()),
            "api".into(),
            "internal".into(),
        );
        let r = app
            .oneshot(
                Request::get(path)
                    .header("authorization", "Bearer api")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let v = body(r).await;
        assert!(v.get("myRank").is_none());
        assert!(v.get("myScore").is_none());
        let seen = f.received.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].1["x-player-credential"], "ranking-secret");
        let decoded = DynamicMessage::decode(
            pool().get_message_by_name(request_name).unwrap(),
            &seen[1].2[5..],
        )
        .unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), request_json);
    }
}
#[tokio::test]
async fn invalid_ranking_requests_never_reach_game() {
    let f = fixture(vec![]).await;
    let app = api::router(
        client(&f, account_config()),
        "api".into(),
        "internal".into(),
    );
    for query in [
        "ranks=",
        "ranks=0",
        "ranks=-1",
        "ranks=1,1",
        "ranks=2147483648",
        "ranks=a",
        "ranks=1&foo=2",
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/events/1/rankings?{query}"))
                    .header("authorization", "Bearer api")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{query}");
    }
    assert!(f.received.lock().unwrap().is_empty());
}

fn whoami_reply(id: &str) -> Reply {
    let mut reply = Reply::version();
    reply.bytes = framed(message("app.player.WhoamiResponse", json!({"playerId":id})));
    reply
}

#[tokio::test]
async fn private_account_data_checks_identity_and_stays_internal() {
    use crate::client::{PLAYER_DATA, WHOAMI};
    let mut reply = Reply::version();
    reply.bytes = framed(message(
        "app.player.GetPlayerDataResponse",
        json!({
            "accountid":"9007199254740993", "worldRoomId":"9223372036854775807",
            "playerData":{"mainDeck":2}, "iapAppleAccountBinding":"private-binding"
        }),
    ));
    let f = fixture(vec![
        Reply::version(),
        whoami_reply("ranking-account"),
        reply,
    ])
    .await;
    let app = api::router(
        client(&f, account_config()),
        "api".into(),
        "internal".into(),
    );
    for (path, token, expected) in [
        ("/internal/v1/account", "api", 401),
        ("/internal/v1/account/player-data", "api", 401),
        ("/internal/other/account", "internal", 404),
        ("/api/v1/account/player-data", "api", 404),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    assert!(f.received.lock().unwrap().is_empty());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/internal/v1/account/player-data")
                .header("authorization", "Bearer internal")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value = body(response).await;
    assert_eq!(value["accountid"], "9007199254740993");
    assert_eq!(value["worldRoomId"], "9223372036854775807");
    assert_eq!(value["playerData"]["mainDeck"], 2);
    let seen = f.received.lock().unwrap();
    assert_eq!(
        seen.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        [VERSION, WHOAMI, PLAYER_DATA]
    );
    assert!(!seen[0].1.contains_key("x-player-credential"));
    for request in &seen[1..] {
        assert_eq!(request.1["x-player-id"], "ranking-account");
        assert_eq!(request.1["x-master-version"], "master-fixture");
        assert_eq!(request.2, framed(vec![]));
    }
}

#[tokio::test]
async fn identity_mismatch_or_failed_auth_never_fetches_private_data() {
    use crate::client::PLAYER_DATA;
    for mut reply in [whoami_reply("wrong-account"), whoami_reply("")] {
        let f = fixture(vec![Reply::version(), reply.clone()]).await;
        assert!(matches!(
            client(&f, account_config())
                .call(PLAYER_DATA, json!({}))
                .await,
            Err(AppError::Protocol)
        ));
        assert_eq!(f.received.lock().unwrap().len(), 2);
        reply.trailers.insert("grpc-status", "16".parse().unwrap());
        let f = fixture(vec![Reply::version(), reply]).await;
        assert!(matches!(
            client(&f, account_config())
                .call(PLAYER_DATA, json!({}))
                .await,
            Err(AppError::Grpc(16))
        ));
        assert_eq!(f.received.lock().unwrap().len(), 2);
    }
    let f = fixture(vec![]).await;
    assert!(matches!(
        client(&f, config()).call(PLAYER_DATA, json!({})).await,
        Err(AppError::AccountUnavailable)
    ));
    assert!(f.received.lock().unwrap().is_empty());
}

fn master_fixture() -> (
    crate::master::Manifest,
    crate::master::MasterDecoder,
    &'static [u8],
) {
    use crate::master::{Entry, Manifest, MasterDecoder};
    let manifest = Manifest {
        version: "fixture-v1".into(),
        files: vec![Entry {
            name: "MasterFixture.bin".into(),
            size: 160,
            hash: "2ab094c0a35d383567fc5582498aae73593d341b0ccb304826d07850c0498def".into(),
        }],
    };
    let decoder = MasterDecoder::new(
        &std::array::from_fn(|i| i as u8),
        std::array::from_fn(|i| (i + 32) as u8),
    );
    (
        manifest,
        decoder,
        include_bytes!("../tests/fixtures/master-synthetic.bin"),
    )
}

#[test]
fn master_decryption_matches_independent_reference_and_rejects_corruption() {
    use crate::master::{key_from_hex, MasterDecoder, MasterError};
    let (manifest, decoder, data) = master_fixture();
    assert_eq!(
        decoder.decode(&manifest.files[0], data).unwrap(),
        include_bytes!("../tests/fixtures/master-synthetic.json")
    );
    let mut corrupt = data.to_vec();
    corrupt[33] ^= 1;
    assert!(matches!(
        decoder.decode(&manifest.files[0], &corrupt),
        Err(MasterError::Integrity)
    ));
    assert!(matches!(
        decoder.decode(&manifest.files[0], &data[..data.len() - 1]),
        Err(MasterError::Integrity)
    ));
    assert!(matches!(
        MasterDecoder::new(&[0; 32], [0; 32]).decode(&manifest.files[0], data),
        Err(MasterError::Cipher)
    ));
    assert!(key_from_hex("1234").is_err());
    assert!(key_from_hex(&"é".repeat(32)).is_err());
    assert_eq!(key_from_hex(&"00".repeat(32)).unwrap(), [0; 32]);
}

#[test]
fn master_manifest_rejects_traversal_duplicates_bad_hash_and_unbounded_files() {
    use crate::master::Manifest;
    let (manifest, _, _) = master_fixture();
    let original = serde_json::to_value(manifest).unwrap();
    for changes in [
        json!({"name":"../MasterFixture.bin"}),
        json!({"name":"Master/Other.bin"}),
        json!({"name":"MasterFixture.json"}),
        json!({"size":33554464}),
        json!({"size":0}),
        json!({"size":65}),
        json!({"hash":"0"}),
    ] {
        let mut value = original.clone();
        value["files"][0]
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    for version in ["../escape", "", "x/y", "x?y"] {
        let mut value = original.clone();
        value["version"] = json!(version);
        assert!(Manifest::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut duplicate = original.clone();
    duplicate["files"]
        .as_array_mut()
        .unwrap()
        .push(original["files"][0].clone());
    assert!(Manifest::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());
}

#[test]
fn master_import_publishes_complete_snapshot_and_preserves_previous_on_failure() {
    use crate::master::import_directory;
    let (mut manifest, decoder, data) = master_fixture();
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input");
    let output = root.path().join("output");
    std::fs::create_dir(&input).unwrap();
    let write_manifest = |manifest: &crate::master::Manifest| {
        std::fs::write(
            input.join("MasterManifest.json"),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap()
    };
    write_manifest(&manifest);
    std::fs::write(input.join("MasterFixture.bin"), data).unwrap();
    let receipt = import_directory(&input, &output, &decoder).unwrap();
    assert_eq!(receipt.tables, 1);
    assert_eq!(
        std::fs::read_to_string(output.join("CURRENT")).unwrap(),
        receipt.snapshot
    );
    assert_eq!(
        std::fs::read(output.join(&receipt.snapshot).join("MasterFixture.json")).unwrap(),
        include_bytes!("../tests/fixtures/master-synthetic.json")
    );
    // A late failure must not replace CURRENT or expose a partial second snapshot.
    let mut absent = manifest.files[0].clone();
    absent.name = "MasterAbsent.bin".into();
    manifest.files.push(absent);
    manifest.version = "fixture-v2".into();
    write_manifest(&manifest);
    assert!(import_directory(&input, &output, &decoder).is_err());
    assert_eq!(
        std::fs::read_to_string(output.join("CURRENT")).unwrap(),
        receipt.snapshot
    );
    assert_eq!(std::fs::read_dir(&output).unwrap().count(), 3);
}

#[tokio::test]
async fn master_http_serves_imported_raw_json_with_version_without_game_calls() {
    let (manifest, decoder, data) = master_fixture();
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input");
    let output = root.path().join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::write(
        input.join("MasterManifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(input.join("MasterFixture.bin"), data).unwrap();
    crate::master::import_directory(&input, &output, &decoder).unwrap();
    let mut cfg = config();
    cfg.master_directory = Some(output.clone());
    let f = fixture(vec![]).await;
    let app = api::router(client(&f, cfg), "api".into(), "internal".into());
    for (path, token, status) in [
        ("/api/v1/master-data", "internal", 401),
        ("/api/other/master", "api", 404),
        ("/api/v1/master-data/tables/MasterMissing", "api", 404),
        ("/api/v1/master-data", "api", 200),
        ("/api/v1/master-data/tables/MasterFixture", "api", 200),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        if status == 200 {
            assert_eq!(response.headers()["x-master-version"], "fixture-v1");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            if path.ends_with("MasterFixture") {
                assert_eq!(
                    bytes.as_ref(),
                    include_bytes!("../tests/fixtures/master-synthetic.json")
                );
            } else {
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap()["tables"],
                    json!(["MasterFixture"])
                );
            }
        }
    }
    std::fs::write(output.join("CURRENT"), "../escape").unwrap();
    assert!(crate::master::read_current(&output, None).is_err());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/master-data")
                .header("authorization", "Bearer api")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(f.received.lock().unwrap().is_empty());
}

async fn cdn_fixture(replies: Vec<Reply>) -> Fixture {
    use axum::{extract::State, routing::any, Router};
    type CdnState = (
        Arc<Mutex<std::collections::VecDeque<Reply>>>,
        ReceivedRequests,
    );
    async fn serve(
        State((replies, seen)): State<CdnState>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let (parts, body) = request.into_parts();
        let body = body.collect().await.unwrap().to_bytes().to_vec();
        seen.lock()
            .unwrap()
            .push((parts.uri.path().into(), parts.headers, body));
        let reply = replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected CDN request");
        tokio::time::sleep(reply.delay).await;
        let mut response = axum::response::Response::builder()
            .status(reply.http_status)
            .body(axum::body::Body::from(reply.bytes))
            .unwrap();
        response.headers_mut().extend(reply.headers);
        response
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .fallback(any(serve))
        .with_state((Arc::new(Mutex::new(replies.into())), received.clone()));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Fixture {
        url,
        received,
        task,
    }
}
fn cdn_reply(bytes: Vec<u8>) -> Reply {
    let mut reply = Reply::version();
    reply.bytes = bytes;
    reply
}
fn remote_master_config(cdn: &Fixture, directory: &std::path::Path) -> Config {
    let mut cfg = config();
    let reference = cfg.cdn_credential_env.values().next().unwrap().clone();
    cfg.default_cdn_root = cdn.url.clone();
    cfg.cdn_credential_env = BTreeMap::from([(cdn.url.clone(), reference)]);
    cfg.master_directory = Some(directory.into());
    let name = format!("SIRIUS_UPDATE_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(format!("{name}_USER"), "fixture-user");
    std::env::set_var(
        format!("{name}_KEY"),
        (0..32).map(|i| format!("{i:02x}")).collect::<String>(),
    );
    std::env::set_var(
        format!("{name}_IV"),
        (32..64).map(|i| format!("{i:02x}")).collect::<String>(),
    );
    cfg.master_update = Some(crate::config::MasterUpdateConfig {
        username_env: format!("{name}_USER"),
        key_hex_env: format!("{name}_KEY"),
        iv_hex_env: format!("{name}_IV"),
        interval_seconds: 60,
    });
    cfg
}
fn remote_master_manifest() -> Vec<u8> {
    let (mut manifest, _, _) = master_fixture();
    manifest.version = "master-fixture".into();
    serde_json::to_vec(&manifest).unwrap()
}

#[tokio::test]
async fn remote_master_flow_downloads_verifies_publishes_and_skips_unchanged_version() {
    use crate::master_update::MasterUpdater;
    let root = tempfile::tempdir().unwrap();
    let cdn = cdn_fixture(vec![
        cdn_reply(remote_master_manifest()),
        cdn_reply(master_fixture().2.to_vec()),
    ])
    .await;
    let game = fixture(vec![Reply::version(), Reply::version(), Reply::version()]).await;
    let cfg = remote_master_config(&cdn, root.path());
    let c = client(&game, cfg.clone());
    let updater = MasterUpdater::new(&cfg, c.clone()).unwrap();
    let result = updater.update_once().await.unwrap();
    assert_eq!(result["action"], "updated");
    assert_eq!(result["receipt"]["source"], "remote");
    let table = crate::master::read_current(root.path(), Some("MasterFixture")).unwrap();
    assert_eq!(
        table.bytes,
        include_bytes!("../tests/fixtures/master-synthetic.json")
    );
    assert_eq!(table.version, "master-fixture");
    let pointer = std::fs::read(root.path().join("CURRENT")).unwrap();
    assert_eq!(updater.update_once().await.unwrap()["action"], "unchanged");
    assert_eq!(std::fs::read(root.path().join("CURRENT")).unwrap(), pointer);
    let status = c.master_update_status().await;
    assert_eq!(status["status"], "ready");
    assert!(!status.to_string().contains("fixture-cdn-secret"));
    let seen = cdn.received.lock().unwrap();
    assert_eq!(
        seen.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        [
            "/master/master-fixture/MasterManifest.json",
            "/master/master-fixture/MasterFixture.bin"
        ]
    );
    for (_, headers, _) in seen.iter() {
        assert_eq!(
            headers["user-agent"],
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            headers["authorization"],
            "Basic Zml4dHVyZS11c2VyOmZpeHR1cmUtY2RuLXNlY3JldA=="
        );
        assert!(!headers.contains_key("x-player-credential"));
        assert!(!headers.contains_key("x-player-id"));
    }
    assert_eq!(game.received.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn remote_master_errors_preserve_installed_snapshot_and_clean_staging() {
    use crate::master_update::MasterUpdater;
    for mode in [
        "corrupt",
        "manifest-version",
        "version-changed",
        "credential-changed",
        "redirect",
        "oversized",
        "unknown-cdn",
        "maintenance",
    ] {
        let root = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let (manifest, decoder, bytes) = master_fixture();
        std::fs::write(
            input.path().join("MasterManifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(input.path().join("MasterFixture.bin"), bytes).unwrap();
        let original =
            crate::master::import_directory(input.path(), root.path(), &decoder).unwrap();
        let mut first = Reply::version();
        if mode == "unknown-cdn" {
            first = first.header("x-sirius-env", "https://unconfigured.example");
        }
        if mode == "maintenance" {
            first.trailers.insert("grpc-status", "2".parse().unwrap());
            first = first.header("x-sirius-error-code", "UNDER_MAINTENANCE");
        }
        let mut second = Reply::version();
        if mode == "version-changed" {
            second.bytes = framed(message(
                "app.masterdata.VersionResponse",
                json!({"version":"new-version"}),
            ));
        }
        if mode == "credential-changed" {
            second = second.header("x-sirius-cred", "rotated-unconfigured-password");
        }
        let mut cdn_replies = Vec::new();
        if !matches!(mode, "unknown-cdn" | "maintenance") {
            let mut reply = cdn_reply(if mode == "manifest-version" {
                serde_json::to_vec(&manifest).unwrap()
            } else {
                remote_master_manifest()
            });
            if mode == "redirect" {
                reply.http_status = 302;
                reply = reply.header("location", "/must-not-follow");
            }
            if mode == "oversized" {
                reply.bytes = vec![b' '; 1024 * 1024 + 1];
            }
            cdn_replies.push(reply);
            if matches!(mode, "corrupt" | "version-changed" | "credential-changed") {
                let mut bytes = bytes.to_vec();
                if mode == "corrupt" {
                    bytes[33] ^= 1;
                }
                cdn_replies.push(cdn_reply(bytes));
            }
        }
        let cdn = cdn_fixture(cdn_replies).await;
        let game = fixture(vec![first, second]).await;
        let cfg = remote_master_config(&cdn, root.path());
        let c = client(&game, cfg.clone());
        assert!(
            MasterUpdater::new(&cfg, c.clone())
                .unwrap()
                .update_once()
                .await
                .is_err(),
            "{mode}"
        );
        assert_eq!(c.master_update_status().await["status"], "failed");
        assert_eq!(
            std::fs::read_to_string(root.path().join("CURRENT")).unwrap(),
            original.snapshot,
            "{mode}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3, "{mode}");
        if matches!(mode, "maintenance" | "unknown-cdn") {
            assert!(cdn.received.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn master_writer_lock_excludes_concurrent_imports_and_releases_on_drop() {
    let root = tempfile::tempdir().unwrap();
    let lock = crate::master::WriterLock::acquire(root.path()).unwrap();
    assert!(crate::master::WriterLock::acquire(root.path()).is_err());
    drop(lock);
    assert!(crate::master::WriterLock::acquire(root.path()).is_ok());
}

#[tokio::test]
async fn remote_master_deadline_releases_writer_and_never_publishes() {
    use crate::master_update::{MasterUpdater, UpdateError};
    let root = tempfile::tempdir().unwrap();
    let mut delayed = cdn_reply(remote_master_manifest());
    delayed.delay = Duration::from_secs(1);
    let cdn = cdn_fixture(vec![delayed]).await;
    let game = fixture(vec![Reply::version()]).await;
    let cfg = remote_master_config(&cdn, root.path());
    let c = client(&game, cfg.clone());
    let mut updater = MasterUpdater::new(&cfg, c.clone()).unwrap();
    MasterUpdater::test_timing(
        &mut updater,
        Duration::from_millis(100),
        Duration::from_secs(60),
    );
    assert!(matches!(
        updater.update_once().await,
        Err(UpdateError::Timeout)
    ));
    assert!(!root.path().join("CURRENT").exists());
    assert_eq!(c.master_update_status().await["status"], "failed");
    assert!(crate::master::WriterLock::acquire(root.path()).is_ok());
}

#[tokio::test]
async fn master_scheduler_runs_checks_then_stops_without_overlapping_writers() {
    use crate::master_update::MasterUpdater;
    let root = tempfile::tempdir().unwrap();
    let cdn = cdn_fixture(vec![
        cdn_reply(remote_master_manifest()),
        cdn_reply(master_fixture().2.to_vec()),
    ])
    .await;
    let game = fixture(vec![Reply::version(), Reply::version(), Reply::version()]).await;
    let cfg = remote_master_config(&cdn, root.path());
    let c = client(&game, cfg.clone());
    let mut updater = MasterUpdater::new(&cfg, c.clone()).unwrap();
    MasterUpdater::test_timing(
        &mut updater,
        Duration::from_secs(5),
        Duration::from_millis(100),
    );
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(updater.run(receiver));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if c.master_update_status().await["result"]["action"] == "unchanged" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(game.received.lock().unwrap().len(), 3);
    assert_eq!(cdn.received.lock().unwrap().len(), 2);
    assert!(crate::master::WriterLock::acquire(root.path()).is_ok());
    let app = api::router(c, "api".into(), "internal".into());
    for (token, status) in [("api", 401), ("internal", 200)] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/internal/v1/master-data/updater")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
    }
}

#[test]
fn updater_configuration_requires_output_and_bounded_interval() {
    let mut cfg = config();
    cfg.master_update = Some(crate::config::MasterUpdateConfig {
        username_env: "U".into(),
        key_hex_env: "K".into(),
        iv_hex_env: "I".into(),
        interval_seconds: 300,
    });
    assert!(cfg.validate().is_err());
    cfg.master_directory = Some("master-data".into());
    assert!(cfg.validate().is_ok());
    for seconds in [0, 59, 86401, u64::MAX] {
        cfg.master_update.as_mut().unwrap().interval_seconds = seconds;
        assert!(cfg.validate().is_err());
    }
}

#[tokio::test]
async fn remote_master_repairs_missing_table_in_matching_version() {
    let root = tempfile::tempdir().unwrap();
    let input = tempfile::tempdir().unwrap();
    let (_, decoder, bytes) = master_fixture();
    std::fs::write(
        input.path().join("MasterManifest.json"),
        remote_master_manifest(),
    )
    .unwrap();
    std::fs::write(input.path().join("MasterFixture.bin"), bytes).unwrap();
    let original = crate::master::import_directory(input.path(), root.path(), &decoder).unwrap();
    std::fs::remove_file(
        root.path()
            .join(&original.snapshot)
            .join("MasterFixture.json"),
    )
    .unwrap();
    let cdn = cdn_fixture(vec![
        cdn_reply(remote_master_manifest()),
        cdn_reply(bytes.to_vec()),
    ])
    .await;
    let game = fixture(vec![Reply::version(), Reply::version()]).await;
    let cfg = remote_master_config(&cdn, root.path());
    let c = client(&game, cfg.clone());
    let result = crate::master_update::MasterUpdater::new(&cfg, c)
        .unwrap()
        .update_once()
        .await
        .unwrap();
    assert_eq!(result["action"], "updated");
    assert_ne!(result["receipt"]["snapshot"], original.snapshot);
    let status = crate::master::read_current(root.path(), None).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&status.bytes).unwrap()["source"],
        "remote"
    );
    assert_eq!(
        crate::master::read_current(root.path(), Some("MasterFixture"))
            .unwrap()
            .bytes,
        include_bytes!("../tests/fixtures/master-synthetic.json")
    );
}

fn copy_protocol_bundle() -> tempfile::TempDir {
    fn copy(source: &std::path::Path, target: &std::path::Path) {
        std::fs::create_dir_all(target).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    copy(
        &crate::config::default_protocol_directory().join("proto"),
        &temp.path().join("proto"),
    );
    std::fs::write(temp.path().join("bundle.json"), r#"{"version":"1.0.3"}"#).unwrap();
    temp
}
fn edit_version_proto(bundle: &std::path::Path, old: &str, new: &str) {
    let file = bundle.join("proto/app/masterdata/masterdata_service.proto");
    let source = std::fs::read_to_string(&file).unwrap();
    assert!(source.contains(old));
    std::fs::write(file, source.replace(old, new)).unwrap();
}

#[test]
fn proto_sources_compile_against_independent_proxy_descriptor_baseline() {
    let loaded =
        crate::protocol::ProtocolBundle::load(&crate::config::default_protocol_directory())
            .unwrap();
    let original = pool();
    assert_eq!(loaded.pool.files().len(), 46);
    assert_eq!(loaded.pool.services().len(), 6);
    assert_eq!(
        loaded.pool.all_messages().len(),
        original.all_messages().len()
    );
    assert_eq!(loaded.pool.all_enums().len(), original.all_enums().len());
    crate::protocol::compatible(&original, &loaded.pool).unwrap();
    crate::protocol::compatible(&loaded.pool, &original).unwrap();
    let msg = DynamicMessage::deserialize(
        loaded
            .pool
            .get_message_by_name("app.friend.FindByProfileIDRequest")
            .unwrap(),
        json!({"playerProfileId":"9007199254740993"}),
    )
    .unwrap();
    assert_eq!(
        msg.encode_to_vec(),
        message(
            "app.friend.FindByProfileIDRequest",
            json!({"playerProfileId":"9007199254740993"})
        )
    );
}

#[tokio::test]
async fn proto_reload_switches_whole_bundle_after_inflight_call_and_invalidates_snapshot() {
    for session_lock in [true, false] {
        check_reload_with_inflight_call(session_lock).await;
    }
}

async fn check_reload_with_inflight_call(session_lock: bool) {
    let directory = copy_protocol_bundle();
    let mut cfg = config();
    cfg.session_lock = session_lock;
    cfg.protocol_directory = directory.path().into();
    // Client starts with the original source schema.
    let mut first = Reply::version().header("x-asset-version", r#"{"version":"v1","iOS":"h1"}"#);
    first.delay = Duration::from_millis(500);
    // The server already emits an additive field, unknown to the initial client.
    let mut extended = message(
        "app.masterdata.VersionResponse",
        json!({"version":"master-fixture"}),
    );
    extended.extend_from_slice(&[0x12, 0x03, b'n', b'e', b'w']); // string field 2
    first.bytes = framed(extended.clone());
    let mut second = Reply::version().header("x-asset-version", r#"{"version":"v2","iOS":"h2"}"#);
    second.bytes = framed(extended);
    let f = fixture(vec![first, second]).await;
    let c = client(&f, cfg);
    let old_status = c.protocol_status().unwrap();
    assert_eq!(old_status.codec, "native");
    let inflight = c.clone();
    let request = tokio::spawn(async move { inflight.call(VERSION, json!({})).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.received.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    edit_version_proto(
        directory.path(),
        "string version = 1;",
        "string version = 1;\n  string extra = 2;",
    );
    std::fs::write(
        directory.path().join("bundle.json"),
        r#"{"version":"1.0.4"}"#,
    )
    .unwrap();
    let reloaded = c.reload_protocol().await.unwrap();
    assert_eq!(reloaded.generation, 2);
    assert_eq!(reloaded.codec, "dynamic");
    assert_eq!(reloaded.version, "1.0.4");
    assert_ne!(old_status.sha256, reloaded.sha256);
    assert!(request.await.unwrap().get("extra").is_none());
    assert_eq!(c.snapshot().await.unwrap()["stale"], true);
    assert!(c.observation().await.master_version.is_none());
    assert_eq!(c.call(VERSION, json!({})).await.unwrap()["extra"], "new");
    assert_eq!(
        c.snapshot().await.unwrap()["snapshot"]["protocol_version"],
        "1.0.4"
    );
    assert_eq!(c.reload_protocol().await.unwrap().generation, 2); // unchanged is idempotent
}

#[tokio::test]
async fn proto_reload_errors_keep_previous_schema_and_auth_is_internal_only() {
    let directory = copy_protocol_bundle();
    let mut cfg = config();
    cfg.protocol_directory = directory.path().into();
    let f = fixture(vec![Reply::version()]).await;
    let c = client(&f, cfg);
    let original = c.protocol_status().unwrap();
    let file = directory
        .path()
        .join("proto/app/masterdata/masterdata_service.proto");
    let source = std::fs::read_to_string(&file).unwrap();
    for candidate in [
        "not valid protobuf".into(),
        source.replace("string version = 1;", "string version = 9;"),
        source.replace("string version = 1;", "int64 version = 1;"),
        source.replace(
            "skip_authentication) = true",
            "skip_authentication) = false",
        ),
        source.replace(
            "returns (.app.masterdata.VersionResponse)",
            "returns (stream .app.masterdata.VersionResponse)",
        ),
        source.replace("rpc Version (", "rpc Renamed ("),
        format!("{source}\nimport \"../private.proto\";"),
    ] {
        std::fs::write(&file, candidate).unwrap();
        assert!(c.reload_protocol().await.is_err());
        assert_eq!(c.protocol_status().unwrap().sha256, original.sha256);
        assert_eq!(c.protocol_status().unwrap().codec, "native");
    }
    // Nested response fields are part of the compatibility boundary too.
    std::fs::write(&file, &source).unwrap();
    let nested = directory.path().join("proto/entity/player_profile.proto");
    let nested_source = std::fs::read_to_string(&nested).unwrap();
    std::fs::write(
        &nested,
        nested_source.replace("int64 profile_id = 6;", "string profile_id = 6;"),
    )
    .unwrap();
    assert!(c.reload_protocol().await.is_err());
    assert_eq!(c.protocol_status().unwrap().sha256, original.sha256);
    assert_eq!(c.protocol_status().unwrap().codec, "native");
    std::fs::write(&nested, nested_source).unwrap();
    std::fs::write(&file, "invalid protobuf").unwrap();
    // Runtime continues to use the old in-memory schema even with invalid files on disk.
    assert_eq!(
        c.call(VERSION, json!({})).await.unwrap()["version"],
        "master-fixture"
    );
    let app = api::router(c, "api".into(), "internal".into());
    for (path, method, token, status) in [
        ("/internal/v1/protocol", "GET", "internal", 200),
        ("/internal/v1/protocol/reload", "POST", "api", 401),
        ("/internal/other/protocol/reload", "POST", "internal", 404),
        ("/internal/v1/protocol/reload", "GET", "internal", 405),
        ("/internal/v1/protocol/reload", "POST", "internal", 422),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        if status == 422 {
            let value = body(response).await;
            assert!(!value.to_string().contains("private.proto"));
        }
    }
    assert_eq!(f.received.lock().unwrap().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn proto_bundle_symlink_deployment_reloads_but_nested_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let original = copy_protocol_bundle();
    let replacement = copy_protocol_bundle();
    let deployment = tempfile::tempdir().unwrap();
    let active = deployment.path().join("current");
    symlink(original.path(), &active).unwrap();
    let mut cfg = config();
    cfg.protocol_directory = active.clone();
    let f = fixture(vec![]).await;
    let c = client(&f, cfg);
    edit_version_proto(
        replacement.path(),
        "string version = 1;",
        "string version = 1;\n string extra = 2;",
    );
    let link = deployment.path().join("next");
    symlink(replacement.path(), &link).unwrap();
    std::fs::rename(link, &active).unwrap();
    assert_eq!(c.reload_protocol().await.unwrap().generation, 2);
    symlink(
        original.path().join("proto/entity/player.proto"),
        replacement.path().join("proto/escape.proto"),
    )
    .unwrap();
    assert!(c.reload_protocol().await.is_err());
    assert_eq!(c.protocol_status().unwrap().generation, 2);
    assert!(f.received.lock().unwrap().is_empty());
}

// Exercise every exposed response tree, including optional presence, enum names /
// unknown enum numbers, repeated messages and 64-bit values beyond JS precision.
fn populated_proto(desc: prost_reflect::MessageDescriptor, depth: usize, unknown: bool) -> Value {
    use prost_reflect::Kind;
    let mut object = serde_json::Map::new();
    if depth == 0 {
        return Value::Object(object);
    }
    for field in desc.fields() {
        assert!(
            !field.is_map(),
            "add map fixture when the exposed schema gains maps"
        );
        let value = match field.kind() {
            Kind::Message(child) => populated_proto(child, depth - 1, unknown),
            Kind::Enum(e) => {
                if unknown {
                    json!(123456)
                } else {
                    json!(e.values().last().unwrap().name())
                }
            }
            Kind::Bool => json!(true),
            Kind::String => json!("fixture 中文"),
            Kind::Bytes => json!("AAH+/w=="),
            Kind::Double | Kind::Float => json!(1.5),
            Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => json!("-9007199254740993"),
            Kind::Uint64 | Kind::Fixed64 => json!("18446744073709551615"),
            _ => json!(123),
        };
        object.insert(
            field.json_name().into(),
            if field.is_list() {
                json!([value])
            } else {
                value
            },
        );
    }
    Value::Object(object)
}

#[test]
fn native_codecs_match_independent_wire_and_dynamic_json_for_all_exposed_routes() {
    let loaded =
        crate::protocol::ProtocolBundle::load(&crate::config::default_protocol_directory())
            .unwrap();
    assert_eq!(loaded.status.codec, "native");
    assert_eq!(loaded.status.sha256, crate::native::SHA256);
    let original = pool();
    for route in crate::protocol::ROUTES {
        let method = crate::protocol::method(&original, route).unwrap();
        for unknown in [false, true] {
            for input in [json!({}), populated_proto(method.input(), 6, unknown)] {
                let expected = DynamicMessage::deserialize(method.input(), input.clone()).unwrap();
                if !unknown {
                    assert!(
                        crate::native::encode("jp", route, &input)
                            .unwrap()
                            .is_some(),
                        "native encode {route}"
                    );
                }
                let encoded = loaded.encode(route, input).unwrap();
                assert_eq!(
                    DynamicMessage::decode(method.input(), encoded.as_slice()).unwrap(),
                    expected,
                    "{route}"
                );
            }
            for output in [json!({}), populated_proto(method.output(), 6, unknown)] {
                let message = DynamicMessage::deserialize(method.output(), output).unwrap();
                let expected = serde_json::to_value(&message).unwrap();
                if !unknown {
                    assert!(
                        crate::native::decode("jp", route, &message.encode_to_vec())
                            .unwrap()
                            .is_some(),
                        "native decode {route}"
                    );
                }
                assert_eq!(
                    loaded.decode(route, &message.encode_to_vec()).unwrap(),
                    expected,
                    "{route}"
                );
            }
        }
        assert!(loaded.decode(route, &[0xff]).is_err());
        assert!(loaded.encode(route, json!({"notAField": true})).is_err());
    }
}

#[test]
fn native_selection_uses_content_not_path_and_dynamic_startup_supports_new_fields() {
    let directory = copy_protocol_bundle();
    let load = || crate::protocol::ProtocolBundle::load(directory.path()).unwrap();
    assert_eq!(load().status.codec, "native");
    edit_version_proto(
        directory.path(),
        "string version = 1;",
        "// comment only\n  string version = 1;",
    );
    assert_eq!(load().status.codec, "native");
    // Same version tag, changed schema: must never select the old native codec.
    edit_version_proto(
        directory.path(),
        "string version = 1;",
        "string version = 1;\n  string extra = 2;",
    );
    let updated = load();
    assert_eq!(updated.status.codec, "dynamic");
    assert_eq!(
        updated.decode(VERSION, &[0x12, 0x01, b'x']).unwrap()["extra"],
        "x"
    );
    // Restoring a complete built-in bundle and restarting returns to native.
    edit_version_proto(directory.path(), "\n  string extra = 2;", "");
    assert_eq!(load().status.codec, "native");
}

#[test]
fn shipped_protocol_and_fixture_contain_only_proxy_dependency_closure() {
    use prost_reflect::Kind;
    use std::collections::BTreeSet;
    let source =
        crate::protocol::ProtocolBundle::load(&crate::config::default_protocol_directory())
            .unwrap();
    for p in [source.pool, pool()] {
        let actual_routes: BTreeSet<_> = p
            .services()
            .flat_map(|service| {
                service
                    .methods()
                    .map(|method| format!("/{}/{}", service.full_name(), method.name()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let expected_routes = crate::protocol::ROUTES
            .iter()
            .map(|route| route.to_string())
            .collect();
        assert_eq!(actual_routes, expected_routes);
        let mut reachable = BTreeSet::new();
        let mut pending = Vec::new();
        for route in crate::protocol::ROUTES {
            let method = crate::protocol::method(&p, route).unwrap();
            pending.extend([
                Kind::Message(method.input()),
                Kind::Message(method.output()),
            ]);
        }
        while let Some(kind) = pending.pop() {
            match kind {
                Kind::Message(message) => {
                    if reachable.insert(message.full_name().to_owned()) {
                        pending.extend(message.fields().map(|field| field.kind()));
                    }
                }
                Kind::Enum(enumeration) => {
                    reachable.insert(enumeration.full_name().to_owned());
                }
                _ => {}
            }
        }
        let shipped: BTreeSet<_> = p
            .all_messages()
            .map(|m| m.full_name().to_owned())
            .chain(p.all_enums().map(|e| e.full_name().to_owned()))
            .filter(|name| !name.starts_with("google.protobuf."))
            .collect();
        assert_eq!(shipped, reachable, "no unused game types may be shipped");
        assert_eq!(
            p.all_extensions()
                .map(|e| e.full_name().to_owned())
                .collect::<Vec<_>>(),
            ["entity.method_options.skip_authentication"]
        );
        assert_eq!(
            p.files()
                .filter(|f| f.name().starts_with("google/"))
                .map(|f| f.name().to_owned())
                .collect::<Vec<_>>(),
            ["google/protobuf/descriptor.proto"]
        );
    }
}

#[test]
fn dotted_resource_versions_remain_single_safe_path_components() {
    assert_eq!(
        resources::select(r#"{"version":"2.3.4.567","iOS":"hash"}"#, "1.0.3")
            .unwrap()
            .0,
        "2.3.4.567"
    );
    for value in [".", "..", "../x", "x/y", "%2e%2e", "x?y", "x#y"] {
        let raw = json!({"version": value, "iOS": "hash"}).to_string();
        assert!(resources::select(&raw, "1.0.3").is_err());
    }
}

#[test]
fn master_versions_accept_version_hash_without_allowing_arbitrary_paths() {
    for version in [
        "fixture",
        "2.3.4.567",
        "2.3.4.567/0123456789abcdef0123456789abcdef",
    ] {
        assert!(crate::master::safe_version(version));
        let input = json!({"version":version,"files":[{"name":"MasterFixture.bin","hash":"a".repeat(64),"size":96}]}).to_string();
        assert!(crate::master::Manifest::parse(input.as_bytes()).is_ok());
    }
    for version in [
        "",
        ".",
        "..",
        "/hash",
        "version/",
        "version/../hash",
        "../hash",
        "version/..",
        "a/b/c",
        "a/%2e%2e",
        "a?x/b",
        "a#b",
    ] {
        assert!(!crate::master::safe_version(version));
    }
    assert!(!crate::master::safe_component("version/hash"));
}

#[test]
fn session_lock_defaults_on_and_accepts_explicit_opt_out() {
    let source = include_str!("../sirius-api-config.example.yaml");
    let omitted = source.replace("session_lock: true", "");
    assert!(
        yaml_serde::from_str::<Config>(&omitted)
            .unwrap()
            .session_lock
    );
    let opted_out = source.replace("session_lock: true", "session_lock: false");
    assert!(
        !yaml_serde::from_str::<Config>(&opted_out)
            .unwrap()
            .session_lock
    );
}

async fn wait_for_requests(f: &Fixture, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.received.lock().unwrap().len() < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_lock_controls_overlap_of_different_authenticated_apis() {
    use crate::client::{PROFILE, WHOAMI};
    for locked in [true, false] {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut identity = whoami_reply("ranking-account");
        identity.gate = Some(gate.clone());
        let mut profile = Reply::version();
        profile.bytes = framed(message("app.friend.FindByProfileIDResponse", json!({})));
        let f = fixture(vec![Reply::version(), identity, profile]).await;
        let mut cfg = account_config();
        cfg.session_lock = locked;
        let c = client(&f, cfg);
        c.call(VERSION, json!({})).await.unwrap();
        let first_client = c.clone();
        let first = tokio::spawn(async move { first_client.call(WHOAMI, json!({})).await });
        wait_for_requests(&f, 2).await;
        let second_client = c.clone();
        let second = tokio::spawn(async move {
            second_client
                .call(PROFILE, json!({"playerProfileId":"123"}))
                .await
        });
        if locked {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(f.received.lock().unwrap().len(), 2);
            assert!(!second.is_finished());
        } else {
            wait_for_requests(&f, 3).await;
            // The second request reached the server while Whoami is still blocked.
            assert!(!first.is_finished());
        }
        gate.add_permits(1);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(f.received.lock().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn unlocked_authenticated_bootstrap_is_single_flight() {
    use crate::client::WHOAMI;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut version = Reply::version();
    version.gate = Some(gate.clone());
    let f = fixture(vec![
        version,
        whoami_reply("ranking-account"),
        whoami_reply("ranking-account"),
    ])
    .await;
    let mut cfg = account_config();
    cfg.session_lock = false;
    let c = client(&f, cfg);
    let a = c.clone();
    let first = tokio::spawn(async move { a.call(WHOAMI, json!({})).await });
    wait_for_requests(&f, 1).await;
    let b = c.clone();
    let second = tokio::spawn(async move { b.call(WHOAMI, json!({})).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.received.lock().unwrap().len(), 1);
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let received = f.received.lock().unwrap();
    assert_eq!(received.len(), 3);
    assert_eq!(received.iter().filter(|r| r.0 == VERSION).count(), 1);
}

#[test]
fn region_config_defaults_and_reserved_cn_fail_closed() {
    use crate::region::{Platform, Region};
    let mut cfg = config();
    assert_eq!(cfg.region, Region::Jp);
    assert_eq!(cfg.platform(), Platform::Ios);
    cfg.region = Region::En;
    cfg.endpoint = "https://l14-prod-va-all-gs-sirius.bilibiligame.net".into();
    cfg.default_cdn_root = "https://cdn.example/prod/en_fixture".into();
    cfg.cdn_credential_env =
        BTreeMap::from([(cfg.default_cdn_root.clone(), "UNSET_EN_CDN".into())]);
    assert_eq!(cfg.platform(), Platform::Android);
    assert!(cfg.validate().is_ok());
    assert_eq!(
        cfg.protocol_path(),
        std::path::PathBuf::from("protocol/global/1.0.1")
    );
    cfg.region = Region::Cn;
    assert!(matches!(cfg.validate(), Err(AppError::Config(_))));
    assert!(yaml_serde::from_str::<crate::region::Region>("global").is_err());
    for bad in [
        "https://cdn.example/prod/../en",
        "https://cdn.example/prod/%2e%2e/en",
        "https://cdn.example/prod/en?token=x",
        "https://cdn.example/prod/en/",
        "https://cdn.example/a\\b",
    ] {
        assert!(!crate::config::cdn_root(bad), "{bad}");
    }
}
#[test]
fn global_native_codec_and_protocol_family_are_independent() {
    let bundle =
        crate::protocol::ProtocolBundle::load(std::path::Path::new("protocol/global/1.0.1"))
            .unwrap();
    assert_eq!(bundle.status.codec, "native");
    assert_eq!(bundle.status.family, "global");
    assert_ne!(bundle.status.sha256, crate::native::SHA256);
    let wire = vec![0x0a, 1, b'm', 0x12, 1, b'r'];
    assert_eq!(
        bundle.decode(VERSION, &wire).unwrap(),
        json!({"version":"m","resourceVersion":"r"})
    );
    let servers = json!({"servers":[{"name":"EN Region","areaId":"3","cdnRoot":"https://cdn.example/prod/en_fixture","apiServerRoot":"https://api.example"}]});
    let method = crate::protocol::method(&bundle.pool, crate::routes::SERVER_LIST).unwrap();
    let bytes = DynamicMessage::deserialize(method.output(), servers.clone())
        .unwrap()
        .encode_to_vec();
    assert_eq!(
        bundle.decode(crate::routes::SERVER_LIST, &bytes).unwrap(),
        servers
    );
    assert!(bundle
        .encode(crate::routes::SERVER_LIST, json!({}))
        .unwrap()
        .is_empty());
    let mut cfg = config();
    cfg.region = crate::region::Region::En;
    cfg.endpoint = "https://api.example".into();
    cfg.default_cdn_root = "https://cdn.example".into();
    cfg.cdn_credential_env =
        BTreeMap::from([(cfg.default_cdn_root.clone(), "UNSET_GLOBAL_CDN".into())]);
    cfg.protocol_directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("protocol/sirius/1.0.3");
    assert!(matches!(
        GameClient::new(cfg),
        Err(AppError::ProtocolDefinition)
    ));
}
#[tokio::test]
async fn global_sends_android_headers_and_refuses_unverified_player_routes() {
    let mut reply = Reply::version();
    reply.bytes = framed(vec![0x0a, 1, b'm', 0x12, 1, b'r']);
    let f = fixture(vec![reply]).await;
    let mut cfg = config();
    cfg.region = crate::region::Region::En;
    cfg.client_version = "1.0.1".into();
    cfg.endpoint = f.url.clone();
    let c = GameClient::for_test(cfg);
    assert!(matches!(
        c.call(crate::routes::PROFILE, json!({"playerProfileId":"1"}))
            .await,
        Err(AppError::UnsupportedRegionOperation)
    ));
    assert!(f.received.lock().unwrap().is_empty());
    assert_eq!(
        c.call(VERSION, json!({})).await.unwrap()["resourceVersion"],
        "r"
    );
    let seen = f.received.lock().unwrap();
    assert_eq!(seen[0].1["x-platform"], "android");
    assert_eq!(seen[0].1["x-client-version"], "1.0.1");
    assert!(!seen[0].1.contains_key("x-player-credential"));
}
#[test]
fn resource_selector_uses_requested_platform_and_never_substitutes_ios() {
    use crate::region::Platform;
    let raw = r#"{"version":"r1","iOS":"ios-hash","Android":"android-hash"}"#;
    assert_eq!(
        resources::select_platform(raw, "1.0.1", Platform::Android).unwrap(),
        ("r1".into(), "android-hash".into())
    );
    assert!(resources::select_platform(
        r#"{"version":"r1","iOS":"ios-hash"}"#,
        "1.0.1",
        Platform::Android
    )
    .is_err());
    assert!(resources::select_platform("unknown", "1.0.1", Platform::Android).is_err());
}

#[test]
fn known_region_endpoints_cannot_be_relabelled() {
    use crate::region::Region;
    for (host, path, region) in [
        ("api.bang-dream-on.jp", "/", Region::Jp),
        (
            "l14-prod-hk-all-gs-sirius.gamerfusiontech.com",
            "/",
            Region::Tw,
        ),
        (
            "l14-prod-va-all-gs-sirius.bilibiligame.net",
            "/",
            Region::En,
        ),
        (
            "l14-prod-kr-all-gs-sirius.bilibiligame.net",
            "/",
            Region::Kr,
        ),
        (
            "l14-prod-sg-patch-sirius.bilibiligame.net",
            "/prod/en_fixture",
            Region::En,
        ),
        (
            "l14-prod-sg-patch-sirius.bilibiligame.net",
            "/prod/kr_fixture",
            Region::Kr,
        ),
    ] {
        for candidate in [Region::Jp, Region::Tw, Region::En, Region::Kr, Region::Cn] {
            assert_eq!(
                candidate.matches_known_service(host, path),
                candidate == region
            );
        }
    }
    let mut cfg = config();
    cfg.region = Region::Tw;
    assert!(matches!(cfg.validate(), Err(AppError::Config(_))));
}
#[tokio::test]
async fn global_hot_reload_preserves_family_and_returns_to_native_on_restore() {
    let temp = tempfile::tempdir().unwrap();
    let source = std::path::Path::new("protocol/global/1.0.1");
    for path in [
        "bundle.json",
        "proto/app/masterdata/masterdata_service.proto",
        "proto/app/playerlogin/playerlogin_service.proto",
        "proto/entity/method_options/method_options.proto",
        "proto/google/protobuf/descriptor.proto",
    ] {
        let target = temp.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source.join(path), target).unwrap();
    }
    let mut cfg = config();
    cfg.region = crate::region::Region::En;
    cfg.protocol_directory = temp.path().into();
    let c = GameClient::for_test(cfg);
    let original = c.protocol_status().unwrap();
    assert_eq!(original.codec, "native");
    edit_version_proto(
        temp.path(),
        "string resource_version = 2;",
        "string resource_version = 2; string fixture_field = 99;",
    );
    let changed = c.reload_protocol().await.unwrap();
    assert_eq!(changed.codec, "dynamic");
    assert_eq!(changed.family, "global");
    std::fs::write(
        temp.path().join("bundle.json"),
        r#"{"version":"1.0.1","family":"jp"}"#,
    )
    .unwrap();
    assert!(c.reload_protocol().await.is_err());
    assert_eq!(c.protocol_status().unwrap().sha256, changed.sha256);
    std::fs::copy(source.join("bundle.json"), temp.path().join("bundle.json")).unwrap();
    std::fs::copy(
        source.join("proto/app/masterdata/masterdata_service.proto"),
        temp.path()
            .join("proto/app/masterdata/masterdata_service.proto"),
    )
    .unwrap();
    // Removing a field after activation is intentionally incompatible: restart restores native.
    assert!(c.reload_protocol().await.is_err());
    let restored = crate::protocol::ProtocolBundle::load(temp.path()).unwrap();
    assert_eq!(restored.status.codec, "native");
    assert_eq!(restored.status.sha256, original.sha256);
}

fn regional_config(region: crate::region::Region) -> Config {
    let mut c = config();
    c.listen = None;
    c.region = region;
    if region != crate::region::Region::Jp {
        c.endpoint = "https://api.example".into();
        c.default_cdn_root = "https://cdn.example".into();
        c.cdn_credential_env =
            BTreeMap::from([("https://cdn.example".into(), "UNSET_FIXTURE_CDN".into())]);
        c.client_version = "1.0.1".into();
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    c.api_token_env = format!("SIRIUS_TEST_API_{id}");
    c.internal_token_env = format!("SIRIUS_TEST_INTERNAL_{id}");
    std::env::set_var(&c.api_token_env, format!("public-{}", region.name()));
    std::env::set_var(&c.internal_token_env, format!("internal-{}", region.name()));
    c
}

#[test]
fn deployment_rejects_ambiguous_region_and_token_scope() {
    use crate::{
        deployment::{DeploymentConfig, MultiConfig},
        region::Region,
    };
    let mut m = MultiConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        regions: BTreeMap::new(),
    };
    assert!(DeploymentConfig::Multi(m.clone()).validate().is_err());
    m.regions.insert("en".into(), regional_config(Region::Jp));
    assert!(DeploymentConfig::Multi(m.clone()).validate().is_err());
    m.regions.clear();
    m.regions.insert("cn".into(), regional_config(Region::Cn));
    assert!(DeploymentConfig::Multi(m.clone()).validate().is_err());
    m.regions.clear();
    let jp = regional_config(Region::Jp);
    let en = regional_config(Region::En);
    // Even across regions, an external bearer cannot gain internal privileges.
    std::env::set_var(&en.internal_token_env, "public-jp");
    m.regions.insert("jp".into(), jp);
    m.regions.insert("en".into(), en);
    assert!(DeploymentConfig::Multi(m.clone()).prepare().is_err());
    m.regions.get_mut("jp").unwrap().listen = Some(m.listen);
    assert!(DeploymentConfig::Multi(m).validate().is_err());
    assert!(DeploymentConfig::parse(
        "listen: 127.0.0.1:9999\nregions: {}\nendpoint: https://example.com"
    )
    .is_err());
    let single =
        DeploymentConfig::parse(include_str!("../sirius-api-config.example.yaml")).unwrap();
    assert_eq!(single.single().unwrap().region, Region::Jp);
}

#[tokio::test]
async fn regional_routes_isolate_authorization_protocol_reload_and_capabilities() {
    use crate::{
        deployment::{DeploymentConfig, MultiConfig},
        region::Region,
    };
    let bundle = copy_protocol_bundle();
    let mut configs = BTreeMap::new();
    for region in [Region::Jp, Region::Tw, Region::En, Region::Kr] {
        let mut c = regional_config(region);
        if region == Region::Jp {
            c.protocol_directory = bundle.path().into();
        }
        configs.insert(region.name().into(), c);
    }
    let deployment = DeploymentConfig::Multi(MultiConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        regions: configs,
    });
    assert!(deployment.single().is_err());
    let app = deployment.prepare().unwrap().router;
    async fn request(app: &axum::Router, path: &str, token: &str, method: &str) -> (u16, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    assert_eq!(request(&app, "/health", "", "GET").await.0, 200);
    for region in [Region::Jp, Region::Tw, Region::En, Region::Kr] {
        let n = region.name();
        let (status, body) = request(
            &app,
            &format!("/api/v1/{n}/regions"),
            &format!("public-{n}"),
            "GET",
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["selected"], n);
        let (status, body) = request(
            &app,
            &format!("/internal/v1/{n}/protocol"),
            &format!("internal-{n}"),
            "GET",
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["family"], region.family());
        assert_eq!(body["generation"], 1);
        assert_eq!(
            request(
                &app,
                &format!("/internal/v1/{n}/protocol"),
                &format!("public-{n}"),
                "GET"
            )
            .await
            .0,
            401
        );
        if region != Region::Jp {
            assert_eq!(
                request(&app, &format!("/api/v1/{n}/regions"), "public-jp", "GET")
                    .await
                    .0,
                401
            );
            assert_eq!(
                request(
                    &app,
                    &format!("/internal/v1/{n}/protocol"),
                    "internal-jp",
                    "GET"
                )
                .await
                .0,
                401
            );
            assert_eq!(
                request(
                    &app,
                    &format!("/api/v1/{n}/players/by-profile-id/123"),
                    &format!("public-{n}"),
                    "GET"
                )
                .await
                .0,
                501
            );
        }
    }
    for path in [
        "/api/v1/regions",
        "/api/v1/cn/regions",
        "/api/v1/global/regions",
    ] {
        assert_eq!(request(&app, path, "public-jp", "GET").await.0, 404);
    }
    edit_version_proto(
        bundle.path(),
        "string version = 1;",
        "string version = 1;\n string extra = 2;",
    );
    let (status, body) = request(
        &app,
        "/internal/v1/jp/protocol/reload",
        "internal-jp",
        "POST",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["generation"], 2);
    assert_eq!(body["codec"], "dynamic");
    for region in ["tw", "en", "kr"] {
        let (_, body) = request(
            &app,
            &format!("/internal/v1/{region}/protocol"),
            &format!("internal-{region}"),
            "GET",
        )
        .await;
        assert_eq!(body["generation"], 1);
        assert_eq!(body["codec"], "native");
    }
}

fn pool_config() -> Config {
    let mut c = config();
    for name in ["one", "two"] {
        let id = format!("SIRIUS_POOL_{}", uuid::Uuid::new_v4().simple());
        let key = format!("{id}_KEY");
        std::env::set_var(&id, format!("player-{name}"));
        std::env::set_var(&key, format!("secret-{name}"));
        c.accounts.push(crate::accounts::AccountConfig {
            name: name.into(),
            player_id_env: Some(id),
            credential_env: Some(key),
            credentials_file: None,
        });
    }
    c
}
fn empty_profile_reply() -> Reply {
    let mut reply = Reply::version();
    reply.bytes = framed(message("app.friend.FindByProfileIDResponse", json!({})));
    reply
}

#[tokio::test]
async fn pool_balances_accounts_and_keeps_each_session_serialized() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut blocked = empty_profile_reply();
    blocked.gate = Some(gate.clone());
    let f = fixture(vec![
        Reply::version(),
        blocked,
        empty_profile_reply(),
        whoami_reply("player-one"),
    ])
    .await;
    let c = client(&f, pool_config());
    c.call(VERSION, json!({})).await.unwrap();
    let a = c.clone();
    let first = tokio::spawn(async move {
        a.call(crate::client::PROFILE, json!({"playerProfileId":"123"}))
            .await
    });
    wait_for_requests(&f, 2).await;
    // A different account can complete while account one is blocked.
    tokio::time::timeout(
        Duration::from_secs(2),
        c.call(crate::client::PROFILE, json!({"playerProfileId":"456"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!first.is_finished());
    let a = c.clone();
    let same = tokio::spawn(async move { a.call_account("one", crate::client::WHOAMI).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(f.received.lock().unwrap().len(), 3);
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    same.await.unwrap().unwrap();
    let seen = f.received.lock().unwrap();
    assert!(!seen[0].1.contains_key("x-player-id"));
    assert_eq!(seen[1].1["x-player-id"], "player-one");
    assert_eq!(seen[2].1["x-player-id"], "player-two");
    assert_eq!(seen[3].1["x-player-id"], "player-one");
    let status = c.account_status().unwrap().to_string();
    assert!(!status.contains("player-one"));
    assert!(!status.contains("secret-one"));
}

#[tokio::test]
async fn pool_disables_failed_auth_without_replaying_request_or_changing_private_identity() {
    let mut denied = empty_profile_reply();
    denied.trailers.insert("grpc-status", "16".parse().unwrap());
    let f = fixture(vec![Reply::version(), denied, empty_profile_reply()]).await;
    let c = client(&f, pool_config());
    assert!(matches!(
        c.call(crate::client::PROFILE, json!({"playerProfileId":"1"}))
            .await,
        Err(AppError::Grpc(16))
    ));
    assert_eq!(f.received.lock().unwrap().len(), 2); // no failover retry
    assert_eq!(c.account_status().unwrap()["accounts"][0]["disabled"], true);
    assert!(matches!(
        c.call(crate::client::WHOAMI, json!({})).await,
        Err(AppError::AccountUnavailable)
    ));
    c.call(crate::client::PROFILE, json!({"playerProfileId":"2"}))
        .await
        .unwrap();
    assert_eq!(f.received.lock().unwrap()[2].1["x-player-id"], "player-two");
    assert!(matches!(
        c.call_account("missing", crate::client::WHOAMI).await,
        Err(AppError::NotFound)
    ));
    let app = api::router(c, "public".into(), "private".into());
    for (path, method, token, expected) in [
        ("/internal/v1/accounts", "GET", "public", 401),
        ("/internal/v1/accounts/reload", "POST", "public", 401),
        (
            "/internal/v1/accounts/one/player-data",
            "GET",
            "public",
            401,
        ),
        ("/internal/v1/accounts", "GET", "private", 200),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

fn write_account_file(path: &std::path::Path, player: &str, key: &str) {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    use std::io::Write;
    write!(temp, "{}", json!({"player_id":player,"credential":key})).unwrap();
    temp.persist(path).unwrap();
}

#[tokio::test]
async fn pool_reload_drains_active_calls_and_rolls_back_invalid_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("account.json");
    write_account_file(&path, "old-player", "old-key");
    let mut cfg = config();
    cfg.accounts.push(crate::accounts::AccountConfig {
        name: "primary".into(),
        player_id_env: None,
        credential_env: None,
        credentials_file: Some(path.clone()),
    });
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut identity = whoami_reply("old-player");
    identity.gate = Some(gate.clone());
    let f = fixture(vec![
        Reply::version(),
        identity,
        whoami_reply("new-player"),
        whoami_reply("new-player"),
    ])
    .await;
    let c = client(&f, cfg);
    let a = c.clone();
    let first = tokio::spawn(async move { a.call_account("primary", crate::client::WHOAMI).await });
    wait_for_requests(&f, 2).await;
    write_account_file(&path, "new-player", "new-key");
    let a = c.clone();
    let reload = tokio::spawn(async move { a.reload_accounts().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!reload.is_finished());
    assert_eq!(c.account_status().unwrap()["generation"], 1);
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    assert_eq!(reload.await.unwrap().unwrap()["generation"], 2);
    c.call_account("primary", crate::client::WHOAMI)
        .await
        .unwrap();
    std::fs::write(&path, "invalid-secret-file").unwrap();
    assert!(c.reload_accounts().await.is_err());
    assert_eq!(c.account_status().unwrap()["generation"], 2);
    c.call_account("primary", crate::client::WHOAMI)
        .await
        .unwrap();
    let seen = f.received.lock().unwrap();
    assert_eq!(seen[1].1["x-player-credential"], "old-key");
    assert_eq!(seen[2].1["x-player-credential"], "new-key");
    assert_eq!(seen[3].1["x-player-credential"], "new-key");
}

#[tokio::test]
async fn pool_timeout_cools_down_and_recovers_without_losing_reservations() {
    let mut cfg = pool_config();
    cfg.accounts.truncate(1);
    cfg.account_pool.failure_threshold = 1;
    cfg.account_pool.cooldown_seconds = 1;
    let mut slow = empty_profile_reply();
    slow.delay = Duration::from_millis(500);
    let f = fixture(vec![Reply::version(), slow, empty_profile_reply()]).await;
    let mut c = client(&f, cfg);
    GameClient::set_test_timeout(&mut c, Duration::from_millis(100));
    c.call(VERSION, json!({})).await.unwrap();
    assert!(matches!(
        c.call(crate::client::PROFILE, json!({"playerProfileId":"1"}))
            .await,
        Err(AppError::Timeout)
    ));
    let status = c.account_status().unwrap();
    assert_eq!(status["accounts"][0]["active_calls"], 0);
    assert_eq!(status["accounts"][0]["consecutive_failures"], 1);
    assert!(matches!(
        c.call(crate::client::PROFILE, json!({"playerProfileId":"1"}))
            .await,
        Err(AppError::AccountUnavailable)
    ));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    c.call(crate::client::PROFILE, json!({"playerProfileId":"1"}))
        .await
        .unwrap();
    assert_eq!(
        c.account_status().unwrap()["accounts"][0]["consecutive_failures"],
        0
    );
}

#[test]
fn pool_rejects_duplicate_identity_mixed_sources_and_unsafe_credentials_files() {
    let mut cfg = pool_config();
    cfg.player_id_env = Some("legacy".into());
    assert!(cfg.validate().is_err());
    cfg.player_id_env = None;
    cfg.accounts[1].name = "one".into();
    assert!(cfg.validate().is_err());
    cfg.accounts[1].name = "two".into();
    std::env::set_var(
        cfg.accounts[1].player_id_env.as_ref().unwrap(),
        "player-one",
    );
    assert!(crate::accounts::Pool::load(&cfg, 1).is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    write_account_file(&path, "file-player", "file-key");
    cfg.accounts.truncate(1);
    cfg.accounts[0].credentials_file = Some(path.clone());
    assert!(cfg.validate().is_err());
    cfg.accounts[0].player_id_env = None;
    cfg.accounts[0].credential_env = None;
    assert!(crate::accounts::Pool::load(&cfg, 1).is_ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(crate::accounts::Pool::load(&cfg, 1).is_err());
    }
}

#[tokio::test]
async fn pool_bootstrap_failure_and_caller_cancellation_do_not_poison_health() {
    let mut failed_version = Reply::version();
    failed_version
        .trailers
        .insert("grpc-status", "16".parse().unwrap());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut blocked = empty_profile_reply();
    blocked.gate = Some(gate.clone());
    let f = fixture(vec![failed_version, Reply::version(), blocked]).await;
    let mut cfg = pool_config();
    cfg.accounts.truncate(1);
    let c = client(&f, cfg);
    assert!(matches!(
        c.call(crate::client::PROFILE, json!({"playerProfileId":"1"}))
            .await,
        Err(AppError::Grpc(16))
    ));
    assert_eq!(
        c.account_status().unwrap()["accounts"][0]["disabled"],
        false
    );
    let a = c.clone();
    let task = tokio::spawn(async move {
        a.call(crate::client::PROFILE, json!({"playerProfileId":"2"}))
            .await
    });
    wait_for_requests(&f, 3).await;
    assert_eq!(
        c.account_status().unwrap()["accounts"][0]["active_calls"],
        1
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let status = c.account_status().unwrap();
    assert_eq!(status["accounts"][0]["active_calls"], 0);
    assert_eq!(status["accounts"][0]["consecutive_failures"], 0);
    tokio::time::timeout(Duration::from_secs(2), c.reload_accounts())
        .await
        .unwrap()
        .unwrap();
    gate.add_permits(1);
}

fn unavailable_reply() -> Reply {
    let mut reply = Reply::version();
    reply.trailers.insert("grpc-status", "14".parse().unwrap());
    reply
}
#[tokio::test]
async fn anonymous_retry_policy_is_bounded_and_preserves_one_deadline() {
    let f = fixture(vec![
        unavailable_reply(),
        unavailable_reply(),
        Reply::version(),
    ])
    .await;
    let mut cfg = config();
    cfg.upstream.anonymous_attempts = 3;
    cfg.upstream.retry_delay_ms = 20;
    let c = client(&f, cfg);
    c.call(VERSION, json!({})).await.unwrap();
    {
        let seen = f.received.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let budgets: Vec<u64> = seen
            .iter()
            .map(|r| {
                r.1["grpc-timeout"]
                    .to_str()
                    .unwrap()
                    .strip_suffix('m')
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        assert!(budgets[0] <= 20_000 && budgets[0] > budgets[1] && budgets[1] > budgets[2]);
        assert!(seen
            .iter()
            .all(|r| !r.1.contains_key("x-player-credential")));
        assert_ne!(seen[0].1["x-request-id"], seen[1].1["x-request-id"]);
    }
    let f = fixture(vec![unavailable_reply(), unavailable_reply()]).await;
    let mut cfg = config();
    cfg.upstream.anonymous_attempts = 2;
    cfg.upstream.retry_delay_ms = 1;
    assert!(matches!(
        client(&f, cfg).call(VERSION, json!({})).await,
        Err(AppError::Grpc(14))
    ));
    assert_eq!(f.received.lock().unwrap().len(), 2);
    let f = fixture(vec![unavailable_reply()]).await;
    let mut cfg = config();
    cfg.upstream.anonymous_attempts = 5;
    cfg.upstream.retry_delay_ms = 1000;
    cfg.upstream.timeout_ms = 100;
    assert!(matches!(
        client(&f, cfg).call(VERSION, json!({})).await,
        Err(AppError::Timeout)
    ));
    assert_eq!(f.received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn retry_policy_never_replays_maintenance_or_authenticated_calls() {
    let f = fixture(vec![
        unavailable_reply().header("x-sirius-error-code", "UNDER_MAINTENANCE")
    ])
    .await;
    let mut cfg = config();
    cfg.upstream.anonymous_attempts = 5;
    assert!(matches!(
        client(&f, cfg).call(VERSION, json!({})).await,
        Err(AppError::Grpc(14))
    ));
    assert_eq!(f.received.lock().unwrap().len(), 1);
    let f = fixture(vec![Reply::version(), unavailable_reply()]).await;
    let mut cfg = account_config();
    cfg.upstream.anonymous_attempts = 5;
    assert!(matches!(
        client(&f, cfg)
            .call(crate::client::PROFILE, json!({"playerProfileId":"1"}))
            .await,
        Err(AppError::Grpc(14))
    ));
    assert_eq!(f.received.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn inflight_limit_queues_calls_inside_their_deadline_and_returns_permits() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut first_reply = Reply::version();
    first_reply.gate = Some(gate.clone());
    let mut slow = Reply::version();
    slow.delay = Duration::from_millis(400);
    let f = fixture(vec![first_reply, slow, Reply::version()]).await;
    let mut cfg = config();
    cfg.session_lock = false;
    cfg.upstream.max_inflight = 1;
    cfg.upstream.timeout_ms = 300;
    let c = client(&f, cfg);
    let a = c.clone();
    let first = tokio::spawn(async move { a.call(VERSION, json!({})).await });
    wait_for_requests(&f, 1).await;
    let a = c.clone();
    let second = tokio::spawn(async move { a.call(VERSION, json!({})).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.received.lock().unwrap().len(), 1);
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    assert!(matches!(second.await.unwrap(), Err(AppError::Timeout)));
    let budget: u64 = f.received.lock().unwrap()[1].1["grpc-timeout"]
        .to_str()
        .unwrap()
        .strip_suffix('m')
        .unwrap()
        .parse()
        .unwrap();
    assert!(budget < 250); // queue time was not reset before sending
    c.call(VERSION, json!({})).await.unwrap();
    assert_eq!(f.received.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn configured_response_limit_is_enforced_on_valid_wire_data() {
    let mut payload = message(
        "app.masterdata.VersionResponse",
        json!({"version":"fixture"}),
    );
    // An unknown length-delimited field is legal Protobuf; independent of decoder support.
    payload.extend_from_slice(&[0x12, 0x80, 0x10]);
    payload.extend(vec![b'x'; 2048]);
    for (limit, success) in [(1024, false), (4096, true)] {
        let mut reply = Reply::version();
        reply.bytes = framed(payload.clone());
        let f = fixture(vec![reply]).await;
        let mut cfg = config();
        cfg.upstream.max_response_bytes = limit;
        let result = client(&f, cfg).call(VERSION, json!({})).await;
        if success {
            assert_eq!(result.unwrap()["version"], "fixture");
        } else {
            assert!(matches!(result, Err(AppError::Protocol)));
        }
    }
}

#[test]
fn upstream_policy_defaults_and_bounds_are_validated() {
    let c = config();
    assert!(c.upstream.validate().is_ok());
    assert_eq!(c.upstream.anonymous_attempts, 1);
    for field in [
        "timeout_ms",
        "max_response_bytes",
        "max_inflight",
        "anonymous_attempts",
        "retry_delay_ms",
    ] {
        let input = format!("{field}: 0");
        let policy: crate::config::UpstreamConfig = yaml_serde::from_str(&input).unwrap();
        assert!(policy.validate().is_err(), "{field}");
    }
    assert!(yaml_serde::from_str::<crate::config::UpstreamConfig>("ignored_option: true").is_err());
}
