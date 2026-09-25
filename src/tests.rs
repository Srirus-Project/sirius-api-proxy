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
        master_sync: None,
        node_routing: None,
        peer_token_env: None,
        asset_dispatch: None,
        logging: None,
        region: crate::region::Region::Jp,
        platform: None,
        protocol_directory: crate::config::default_protocol_directory(),
        listen: Some("127.0.0.1:0".parse().unwrap()),
        tls: None,
        access_log: None,
        environment: "release".into(),
        endpoint: "https://api.bang-dream-on.jp".into(),
        client_version: "1.0.3".into(),
        session_lock: true,
        upstream: Default::default(),
        response_cache: Default::default(),
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
        network: Default::default(),
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
        network: Default::default(),
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
        logging: None,
        tls: None,
        access_log: None,
        listen: "127.0.0.1:0".parse().unwrap(),
        regions: BTreeMap::new(),
    };
    assert!(DeploymentConfig::Multi(Box::new(m.clone()))
        .validate()
        .is_err());
    m.regions.insert("en".into(), regional_config(Region::Jp));
    assert!(DeploymentConfig::Multi(Box::new(m.clone()))
        .validate()
        .is_err());
    m.regions.clear();
    m.regions.insert("cn".into(), regional_config(Region::Cn));
    assert!(DeploymentConfig::Multi(Box::new(m.clone()))
        .validate()
        .is_err());
    m.regions.clear();
    let jp = regional_config(Region::Jp);
    let en = regional_config(Region::En);
    // Even across regions, an external bearer cannot gain internal privileges.
    std::env::set_var(&en.internal_token_env, "public-jp");
    m.regions.insert("jp".into(), jp);
    m.regions.insert("en".into(), en);
    assert!(DeploymentConfig::Multi(Box::new(m.clone()))
        .prepare()
        .is_err());
    m.regions.get_mut("jp").unwrap().access_log = Some(Default::default());
    assert!(DeploymentConfig::Multi(Box::new(m.clone()))
        .validate()
        .is_err());
    m.regions.get_mut("jp").unwrap().access_log = None;
    m.regions.get_mut("jp").unwrap().listen = Some(m.listen);
    assert!(DeploymentConfig::Multi(Box::new(m)).validate().is_err());
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
    let deployment = DeploymentConfig::Multi(Box::new(MultiConfig {
        logging: None,
        tls: None,
        access_log: None,
        listen: "127.0.0.1:0".parse().unwrap(),
        regions: configs,
    }));
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

fn memory_cache(ttl_ms: u64) -> crate::response_cache::Config {
    crate::response_cache::Config::Memory {
        stale_while_revalidate_ms: 0,
        route_ttl_ms: BTreeMap::new(),
        ttl_ms,
        max_entries: 4,
        max_bytes: 8192,
        max_entry_bytes: 4096,
    }
}
fn ranking_reply(score: i32) -> Reply {
    let mut reply = Reply::version();
    reply.bytes = framed(message(
        "app.livemusic.GetRankingResponse",
        json!({"myRank":123,"players":[{"score":score}]}),
    ));
    reply
}
#[tokio::test]
async fn response_cache_hits_are_public_and_account_protocol_reload_invalidate() {
    let bundle = copy_protocol_bundle();
    let f = fixture(vec![
        Reply::version(),
        ranking_reply(1),
        ranking_reply(2),
        Reply::version(),
        ranking_reply(3),
        whoami_reply("ranking-account"),
        whoami_reply("ranking-account"),
        empty_profile_reply(),
        empty_profile_reply(),
    ])
    .await;
    let mut cfg = account_config();
    cfg.response_cache = memory_cache(5000);
    cfg.protocol_directory = bundle.path().into();
    let c = client(&f, cfg);
    let route = crate::client::MUSIC_RANKING;
    for _ in 0..2 {
        let value = c.call(route, json!({"musicId":"1"})).await.unwrap();
        assert!(value.get("myRank").is_none());
        assert_eq!(value["players"][0]["score"], 1);
    }
    assert_eq!(f.received.lock().unwrap().len(), 2);
    c.reload_accounts().await.unwrap();
    assert_eq!(
        c.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        2
    );
    edit_version_proto(
        bundle.path(),
        "string version = 1;",
        "string version = 1;\n string extra = 2;",
    );
    c.reload_protocol().await.unwrap();
    assert_eq!(
        c.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        3
    );
    for _ in 0..2 {
        c.call(crate::client::WHOAMI, json!({})).await.unwrap();
    }
    for _ in 0..2 {
        c.call(crate::client::PROFILE, json!({"playerProfileId":"2"}))
            .await
            .unwrap();
    }
    assert_eq!(f.received.lock().unwrap().len(), 9);
}

#[tokio::test]
async fn response_cache_does_not_store_failures_or_reuse_different_inputs() {
    let f = fixture(vec![
        Reply::version(),
        unavailable_reply(),
        ranking_reply(1),
        ranking_reply(2),
    ])
    .await;
    let mut cfg = account_config();
    cfg.response_cache = memory_cache(5000);
    let c = client(&f, cfg);
    let route = crate::client::MUSIC_RANKING;
    assert!(c.call(route, json!({"musicId":"1"})).await.is_err());
    assert_eq!(
        c.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        1
    );
    assert_eq!(
        c.call(route, json!({"musicId":"2"})).await.unwrap()["players"][0]["score"],
        2
    );
    assert_eq!(
        c.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        1
    );
    assert_eq!(f.received.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn memory_response_cache_enforces_ttl_entry_and_total_bounds() {
    use crate::response_cache::{Cache, Config};
    let c = Cache::new(Config::Memory {
        stale_while_revalidate_ms: 0,
        route_ttl_ms: BTreeMap::new(),
        ttl_ms: 30,
        max_entries: 1,
        max_bytes: 1024,
        max_entry_bytes: 512,
    })
    .unwrap();
    c.put("first".into(), &json!({"value":1})).await;
    assert_eq!(c.get("first").await.unwrap()["value"], 1);
    c.put("second".into(), &json!({"value":2})).await;
    assert!(c.get("first").await.is_none());
    assert_eq!(c.get("second").await.unwrap()["value"], 2);
    c.put("oversized".into(), &json!({"value":"x".repeat(1024)}))
        .await;
    assert!(c.get("oversized").await.is_none());
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(c.get("second").await.is_none());
    assert!(Config::Memory {
        stale_while_revalidate_ms: 0,
        route_ttl_ms: BTreeMap::new(),
        ttl_ms: 0,
        max_entries: 1,
        max_bytes: 1024,
        max_entry_bytes: 512
    }
    .validate()
    .is_err());
}

#[tokio::test]
#[ignore = "requires SIRIUS_TEST_REDIS_SERVER to run an isolated local Redis instance"]
async fn redis_response_cache_bounds_reads_expires_and_fails_open_on_outage() {
    use crate::response_cache::{Cache, Config};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Child(
        std::process::Command::new(std::env::var("SIRIUS_TEST_REDIS_SERVER").unwrap())
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--save",
                "",
                "--appendonly",
                "no",
                "--dir",
                root.path().to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let url = format!("redis://127.0.0.1:{port}/");
    let raw = redis::Client::open(url.clone()).unwrap();
    let mut ready = None;
    for _ in 0..50 {
        if let Ok(c) = raw.get_multiplexed_async_connection().await {
            ready = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut connection = ready.expect("Redis became ready");
    let env = format!("SIRIUS_REDIS_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&env, url);
    let c = Cache::new(Config::Redis {
        stale_while_revalidate_ms: 0,
        route_ttl_ms: BTreeMap::from([(crate::response_cache::Route::SongRankings, 2000)]),
        url_env: env.clone(),
        namespace: "test".into(),
        ttl_ms: 100,
        max_entry_bytes: 1024,
        operation_timeout_ms: 100,
    })
    .unwrap();
    c.put("safe".into(), &json!({"value":42})).await;
    assert_eq!(c.get("safe").await.unwrap()["value"], 42);
    c.put_route(
        crate::client::MUSIC_RANKING,
        "override".into(),
        &json!({"value":43}),
    )
    .await;
    let _: () = redis::cmd("SET")
        .arg("test:oversized")
        .arg("x".repeat(100_000))
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!(c.get("oversized").await.is_none());
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(c.get("safe").await.is_none());
    assert_eq!(c.get("override").await.unwrap()["value"], 43);
    let remaining: i64 = redis::cmd("PTTL")
        .arg("test:override")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!((100..=2000).contains(&remaining));

    let f = fixture(vec![
        Reply::version(),
        ranking_reply(10),
        Reply::version(),
        ranking_reply(20),
    ])
    .await;
    let stale = Cache::new(Config::Redis {
        stale_while_revalidate_ms: 200,
        route_ttl_ms: BTreeMap::new(),
        url_env: env.clone(),
        namespace: "stale-test".into(),
        ttl_ms: 20,
        max_entry_bytes: 4096,
        operation_timeout_ms: 100,
    })
    .unwrap();
    stale.put("value".into(), &json!({"value":1})).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(stale.get("value").await.is_none());
    assert!(stale.get_with_state("value").await.unwrap().stale);
    let remaining: i64 = redis::cmd("PTTL")
        .arg("stale-test:value")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!((1..=200).contains(&remaining));
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert!(stale.get_with_state("value").await.is_none());
    let mut first_config = account_config();
    first_config.response_cache = Config::Redis {
        stale_while_revalidate_ms: 0,
        route_ttl_ms: BTreeMap::new(),
        url_env: env,
        namespace: "client-test".into(),
        ttl_ms: 5000,
        max_entry_bytes: 4096,
        operation_timeout_ms: 100,
    };
    let mut second_config = account_config();
    second_config.response_cache = first_config.response_cache.clone();
    std::env::set_var(
        second_config.player_id_env.as_ref().unwrap(),
        "other-fixture-account",
    );
    std::env::set_var(
        second_config.player_credential_env.as_ref().unwrap(),
        "other-fixture-credential",
    );
    let first = client(&f, first_config);
    let second = client(&f, second_config);
    let route = crate::client::MUSIC_RANKING;
    for _ in 0..2 {
        let result = first.call(route, json!({"musicId":"1"})).await.unwrap();
        assert_eq!(result["players"][0]["score"], 10);
        assert!(result.get("myRank").is_none());
    }
    assert_eq!(
        second.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        20
    );
    assert_eq!(f.received.lock().unwrap().len(), 4);
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        assert!(c.get("outage").await.is_none());
        c.put("outage".into(), &json!({"value":1})).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn route_cache_ttls_expire_independently_and_zero_bypasses_rpc_cache() {
    use crate::response_cache::{Cache, Config, Route};
    let cache = Cache::new(Config::Memory {
        stale_while_revalidate_ms: 0,
        ttl_ms: 5000,
        max_entries: 10,
        max_bytes: 8192,
        max_entry_bytes: 4096,
        route_ttl_ms: BTreeMap::from([(Route::Announcement, 20), (Route::EventRankings, 0)]),
    })
    .unwrap();
    cache
        .put_route(ANNOUNCEMENT, "short".into(), &json!({"value":1}))
        .await;
    cache
        .put_route(
            crate::client::MUSIC_RANKING,
            "long".into(),
            &json!({"value":2}),
        )
        .await;
    cache
        .put_route(
            crate::client::EVENT_RANKING,
            "disabled".into(),
            &json!({"value":3}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(cache.get("short").await.is_none());
    assert!(cache.get("disabled").await.is_none());
    assert_eq!(cache.get("long").await.unwrap()["value"], 2);
    assert!(cache.ttl(crate::client::PROFILE).is_none());
    assert!(cache.ttl(crate::client::PLAYER_DATA).is_none());
    let f = fixture(vec![Reply::version(), ranking_reply(1), ranking_reply(2)]).await;
    let mut cfg = account_config();
    cfg.response_cache = memory_cache(5000);
    if let Config::Memory { route_ttl_ms, .. } = &mut cfg.response_cache {
        route_ttl_ms.insert(Route::SongRankings, 0);
    }
    let c = client(&f, cfg);
    for score in [1, 2] {
        assert_eq!(
            c.call(crate::client::MUSIC_RANKING, json!({"musicId":"1"}))
                .await
                .unwrap()["players"][0]["score"],
            score
        );
    }
    assert_eq!(f.received.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn concurrent_cache_misses_coalesce_and_cancelled_fill_releases_waiters() {
    for cancel_first in [false, true] {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut blocked = ranking_reply(1);
        blocked.gate = Some(gate.clone());
        let mut replies = vec![Reply::version(), blocked];
        if cancel_first {
            replies.push(ranking_reply(2));
        }
        let f = fixture(replies).await;
        let mut cfg = account_config();
        cfg.session_lock = false;
        cfg.response_cache = memory_cache(5000);
        let c = client(&f, cfg);
        c.call(VERSION, json!({})).await.unwrap();
        let a = c.clone();
        let first = tokio::spawn(async move {
            a.call(crate::client::MUSIC_RANKING, json!({"musicId":"1"}))
                .await
        });
        wait_for_requests(&f, 2).await;
        let a = c.clone();
        let second = tokio::spawn(async move {
            a.call(crate::client::MUSIC_RANKING, json!({"musicId":"1"}))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.received.lock().unwrap().len(), 2);
        assert!(!second.is_finished());
        if cancel_first {
            first.abort();
            assert!(first.await.unwrap_err().is_cancelled());
        } else {
            gate.add_permits(1);
            first.await.unwrap().unwrap();
        }
        let value = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            value["players"][0]["score"],
            if cancel_first { 2 } else { 1 }
        );
        assert_eq!(
            f.received.lock().unwrap().len(),
            if cancel_first { 3 } else { 2 }
        );
        gate.add_permits(1);
    }
}

#[test]
fn cache_route_policy_rejects_unknown_or_private_routes_and_excessive_ttls() {
    let prefix = "backend: memory\nttl_ms: 1000\nmax_entries: 4\nmax_bytes: 4096\nmax_entry_bytes: 1024\nroute_ttl_ms:\n";
    for route in ["profile", "player_data", "arbitrary_rpc"] {
        assert!(
            yaml_serde::from_str::<crate::response_cache::Config>(&format!(
                "{prefix}  {route}: 100"
            ))
            .is_err()
        );
    }
    let too_long: crate::response_cache::Config =
        yaml_serde::from_str(&format!("{prefix}  announcements: 300001")).unwrap();
    assert!(too_long.validate().is_err());
    let disabled: crate::response_cache::Config =
        yaml_serde::from_str(&format!("{prefix}  announcements: 0")).unwrap();
    disabled.validate().unwrap();
}

struct TunnelProxy {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for TunnelProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn tunnel_proxy(response: Vec<u8>, forward: bool, delay: Duration) -> TunnelProxy {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let response = response.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut bytes = vec![];
                while bytes.len() < 16384 && !bytes.ends_with(b"\r\n\r\n") {
                    let Ok(byte) = stream.read_u8().await else {
                        return;
                    };
                    bytes.push(byte);
                }
                let header = String::from_utf8(bytes).unwrap();
                requests.lock().unwrap().push(header.clone());
                tokio::time::sleep(delay).await;
                if stream.write_all(&response).await.is_err() {
                    return;
                }
                if forward {
                    let address: std::net::SocketAddr =
                        header.split_whitespace().nth(1).unwrap().parse().unwrap();
                    assert!(address.ip().is_loopback());
                    let mut upstream = tokio::net::TcpStream::connect(address).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                }
            });
        }
    });
    TunnelProxy { url, seen, task }
}
fn proxy_policy(url: &str) -> crate::config::UpstreamConfig {
    let name = format!("TEST_PROXY_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&name, url);
    crate::config::UpstreamConfig {
        proxy_url_env: Some(name),
        ..Default::default()
    }
}
#[tokio::test]
async fn connect_proxy_preserves_http2_trailers_and_separates_proxy_and_account_headers() {
    let proxy = tunnel_proxy(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Connection established\r\n\r\n".to_vec(),
        true,
        Duration::ZERO,
    )
    .await;
    let f = fixture(vec![
        Reply::version(),
        empty_profile_reply(),
        unavailable_reply(),
    ])
    .await;
    let mut cfg = config();
    cfg.upstream = proxy_policy(&proxy.url);
    let auth = format!("PROXY_AUTH_{}", uuid::Uuid::new_v4().simple());
    let id = format!("PROXY_PLAYER_{}", uuid::Uuid::new_v4().simple());
    let key = format!("{id}_KEY");
    std::env::set_var(&auth, "Basic synthetic-proxy-only");
    std::env::set_var(&id, "fixture-player");
    std::env::set_var(&key, "fixture-account-secret");
    cfg.upstream.proxy_authorization_env = Some(auth);
    cfg.player_id_env = Some(id);
    cfg.player_credential_env = Some(key);
    let c = client(&f, cfg);
    c.call(crate::routes::PROFILE, json!({"playerProfileId":"1"}))
        .await
        .unwrap();
    assert!(matches!(
        c.call(VERSION, json!({})).await,
        Err(AppError::Grpc(14))
    ));
    let tunnels = proxy.seen.lock().unwrap();
    assert_eq!(tunnels.len(), 1, "HTTP/2 connection should be reused");
    assert!(tunnels[0].starts_with(&format!(
        "CONNECT {} HTTP/1.1\r\n",
        f.url.strip_prefix("http://").unwrap()
    )));
    assert!(tunnels[0].contains("Proxy-Authorization: Basic synthetic-proxy-only\r\n"));
    assert!(!tunnels[0].contains("fixture-account-secret"));
    let seen = f.received.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[1].1["x-player-credential"], "fixture-account-secret");
    assert!(seen
        .iter()
        .all(|r| !r.1.contains_key("proxy-authorization")));
}
#[tokio::test]
async fn failed_proxy_never_falls_back_to_origin_and_handshake_is_bounded() {
    for response in [
        b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n".to_vec(),
        b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1\r\n\r\n".to_vec(),
        b"HTTP/1.1 101 Switching Protocols\r\n\r\n".to_vec(),
        b"NOT HTTP\r\n\r\n".to_vec(),
        [
            b"HTTP/1.1 200 OK\r\nX: ".to_vec(),
            vec![b'x'; 16384],
            b"\r\n\r\n".to_vec(),
        ]
        .concat(),
    ] {
        let f = fixture(vec![]).await;
        let proxy = tunnel_proxy(response, false, Duration::ZERO).await;
        let mut cfg = config();
        cfg.upstream = proxy_policy(&proxy.url);
        cfg.upstream.anonymous_attempts = 3;
        assert!(matches!(
            client(&f, cfg).call(VERSION, json!({})).await,
            Err(AppError::Proxy)
        ));
        assert_eq!(proxy.seen.lock().unwrap().len(), 1);
        assert!(f.received.lock().unwrap().is_empty());
    }
    let f = fixture(vec![]).await;
    let proxy = tunnel_proxy(vec![], false, Duration::from_secs(1)).await;
    let mut cfg = config();
    cfg.upstream = proxy_policy(&proxy.url);
    cfg.upstream.connect_timeout_ms = 100;
    let result = tokio::time::timeout(
        Duration::from_millis(800),
        client(&f, cfg).call(VERSION, json!({})),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(AppError::Transport)));
    assert!(f.received.lock().unwrap().is_empty());
    let mut cfg = config();
    cfg.upstream = proxy_policy(&proxy.url);
    cfg.upstream.timeout_ms = 100;
    assert!(matches!(
        client(&f, cfg).call(VERSION, json!({})).await,
        Err(AppError::Timeout)
    ));
}
#[tokio::test]
async fn independent_clients_use_only_their_configured_proxy() {
    let first = tunnel_proxy(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), true, Duration::ZERO).await;
    let second = tunnel_proxy(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), true, Duration::ZERO).await;
    let f = fixture(vec![Reply::version(), Reply::version(), Reply::version()]).await;
    for proxy in [Some(&first), Some(&second), None] {
        let mut cfg = config();
        if let Some(proxy) = proxy {
            cfg.upstream = proxy_policy(&proxy.url);
        }
        client(&f, cfg).call(VERSION, json!({})).await.unwrap();
    }
    assert_eq!(first.seen.lock().unwrap().len(), 1);
    assert_eq!(second.seen.lock().unwrap().len(), 1);
    assert_eq!(f.received.lock().unwrap().len(), 3);
}
#[test]
fn proxy_configuration_rejects_unsupported_urls_and_header_injection_without_echoing_secrets() {
    for value in [
        "socks5://127.0.0.1:1080",
        "http://user:secret@127.0.0.1",
        "http://localhost/path",
        "http://localhost?secret",
        "https://localhost#secret",
        " http://localhost",
        "http://localhost\\secret",
    ] {
        let error = match crate::transport::Connector::new(&proxy_policy(value)) {
            Ok(_) => panic!("invalid proxy accepted"),
            Err(e) => e,
        };
        assert!(!error.to_string().contains("secret"));
    }
    for url in [
        "http://127.0.0.1:8080",
        "https://localhost:8443",
        "http://[::1]:8080",
    ] {
        assert!(crate::transport::Connector::new(&proxy_policy(url)).is_ok());
    }
    let mut policy = crate::config::UpstreamConfig {
        proxy_authorization_env: Some("UNUSED".into()),
        ..Default::default()
    };
    assert!(policy.validate().is_err());
    policy = proxy_policy("http://127.0.0.1:8080");
    let name = format!("BAD_PROXY_AUTH_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&name, "Basic test\r\nX-Leak: secret");
    policy.proxy_authorization_env = Some(name);
    assert!(crate::transport::Connector::new(&policy).is_err());
}

async fn untrusted_tls_fixture() -> (String, tokio::task::JoinHandle<bool>) {
    let cert = rustls::pki_types::CertificateDer::from(
        include_bytes!("../tests/fixtures/untrusted-localhost.der").to_vec(),
    );
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
        include_bytes!("../tests/fixtures/untrusted-localhost-key.der").to_vec(),
    );
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert], key.into())
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(stream)
            .await
            .is_err()
    });
    (url, task)
}
#[tokio::test]
async fn proxy_and_origin_tls_both_reject_untrusted_certificates() {
    // A proxy's TLS certificate must be checked before any CONNECT credentials.
    let (url, rejected) = untrusted_tls_fixture().await;
    let f = fixture(vec![]).await;
    let mut cfg = config();
    cfg.upstream = proxy_policy(&url);
    assert!(matches!(
        client(&f, cfg).call(VERSION, json!({})).await,
        Err(AppError::Transport)
    ));
    assert!(tokio::time::timeout(Duration::from_secs(2), rejected)
        .await
        .unwrap()
        .unwrap());
    assert!(f.received.lock().unwrap().is_empty());
    // Origin TLS remains enabled after a successful plaintext proxy tunnel.
    let (url, rejected) = untrusted_tls_fixture().await;
    let proxy = tunnel_proxy(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), true, Duration::ZERO).await;
    let mut cfg = config();
    cfg.endpoint = url;
    cfg.upstream = proxy_policy(&proxy.url);
    let c = GameClient::new(cfg).unwrap();
    assert!(matches!(
        c.call(VERSION, json!({})).await,
        Err(AppError::Transport)
    ));
    assert!(tokio::time::timeout(Duration::from_secs(2), rejected)
        .await
        .unwrap()
        .unwrap());
    assert_eq!(proxy.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn connect_authority_keeps_dns_remote_ipv6_ports_and_tunnel_bytes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower_service::Service;
    for (target, expected) in [
        (
            "https://not-resolvable.invalid",
            "not-resolvable.invalid:443",
        ),
        ("https://[::1]", "[::1]:443"),
        ("https://example.invalid:9443", "example.invalid:9443"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let policy = proxy_policy(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = vec![];
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
            }
            assert!(String::from_utf8(header)
                .unwrap()
                .starts_with(&format!("CONNECT {expected} HTTP/1.1\r\n")));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 123\r\n\r\ntunnel")
                .await
                .unwrap();
        });
        let stream = crate::transport::Connector::new(&policy)
            .unwrap()
            .call(target.parse().unwrap())
            .await
            .unwrap();
        let mut stream = TokioIo::new(stream);
        let mut bytes = [0; 6];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"tunnel");
        server.await.unwrap();
    }
}
#[tokio::test]
async fn origin_tls_handshake_obeys_connection_timeout() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config();
    cfg.endpoint = format!("https://{}", listener.local_addr().unwrap());
    cfg.upstream.connect_timeout_ms = 100;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 22); // TLS handshake record.
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let c = GameClient::new(cfg).unwrap();
    let result = tokio::time::timeout(Duration::from_millis(800), c.call(VERSION, json!({})))
        .await
        .unwrap();
    assert!(matches!(result, Err(AppError::Transport)));
    server.abort();
    let _ = server.await;
}

fn listener_tls_config(root: &std::path::Path) -> crate::server::TlsConfig {
    let cert = root.join("cert.pem");
    let key = root.join("key.pem");
    std::fs::write(&cert, include_bytes!("../tests/fixtures/listener-cert.pem")).unwrap();
    std::fs::write(&key, include_bytes!("../tests/fixtures/listener-key.pem")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    crate::server::TlsConfig {
        certificate_file: cert,
        private_key_file: key,
        handshake_timeout_ms: 100,
    }
}
#[test]
fn listener_tls_rejects_invalid_mismatched_oversized_and_unsafe_key_files() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = listener_tls_config(root.path());
    assert!(cfg.load().is_ok());
    cfg.handshake_timeout_ms = 0;
    assert!(cfg.load().is_err());
    cfg.handshake_timeout_ms = 100;
    std::fs::write(
        &cfg.certificate_file,
        include_bytes!("../tests/fixtures/listener-other-cert.pem"),
    )
    .unwrap();
    assert!(cfg.load().is_err()); // Valid PEM, but its public key does not match.
    std::fs::write(
        &cfg.certificate_file,
        include_bytes!("../tests/fixtures/listener-cert.pem"),
    )
    .unwrap();
    std::fs::write(&cfg.private_key_file, b"invalid private key").unwrap();
    let error = cfg.load().err().unwrap().to_string();
    assert!(
        !error.contains("invalid private key") && !error.contains(root.path().to_str().unwrap())
    );
    std::fs::write(&cfg.private_key_file, vec![b'x'; 128 * 1024 + 1]).unwrap();
    assert!(cfg.load().is_err());
    let cfg = listener_tls_config(root.path());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &cfg.private_key_file,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(cfg.load().is_err());
        std::fs::set_permissions(
            &cfg.private_key_file,
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert!(cfg.load().is_ok());
    }
    std::fs::remove_file(&cfg.private_key_file).unwrap();
    assert!(cfg.load().is_err());
}
#[tokio::test]
async fn https_listener_preserves_auth_http2_peer_address_and_graceful_shutdown() {
    use tokio::io::AsyncReadExt;
    let root = tempfile::tempdir().unwrap();
    let cfg = listener_tls_config(root.path());
    let tls = cfg.load().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let entered = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let handler_entered = entered.clone();
    let handler_release = release.clone();
    let router = axum::Router::new()
        .route(
            "/slow",
            axum::routing::get(move || {
                let entered = handler_entered.clone();
                let release = handler_release.clone();
                async move {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    "drained"
                }
            }),
        )
        .route("/health", axum::routing::get(|| async { "ok" }))
        .route(
            "/private",
            axum::routing::get(
                |headers: axum::http::HeaderMap,
                 axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<
                    std::net::SocketAddr,
                >| async move {
                    if headers
                        .get("authorization")
                        .is_some_and(|v| v == "Bearer listener-test")
                    {
                        (axum::http::StatusCode::OK, peer.ip().to_string())
                    } else {
                        (axum::http::StatusCode::UNAUTHORIZED, "unauthorized".into())
                    }
                },
            ),
        );
    let log_path = root.path().join("listener-access.log");
    let access = crate::access_log::AccessLog::new(access_log_config(log_path.clone())).unwrap();
    let router = access.wrap(router);
    let (stop, signal) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(crate::server::serve(listener, router, Some(tls), async {
        let _ = signal.await;
    }));
    let certificate =
        reqwest::Certificate::from_pem(include_bytes!("../tests/fixtures/listener-cert.pem"))
            .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .tls_certs_only([certificate])
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("https://{address}");
    assert_eq!(
        client
            .get(format!("{url}/health"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    assert_eq!(
        client
            .get(format!("{url}/private"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = client
        .get(format!("{url}/private"))
        .bearer_auth("listener-test")
        .header("x-forwarded-for", "198.51.100.44")
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.text().await.unwrap(), "127.0.0.1");
    let untrusted = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert!(untrusted.get(format!("{url}/health")).send().await.is_err());
    assert!(client
        .get(format!("http://{address}/health"))
        .send()
        .await
        .is_err());
    // A silent TCP peer cannot occupy a TLS handshake indefinitely.
    let mut stalled = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stalled.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let request = client.get(format!("{url}/slow"));
    let pending = tokio::spawn(async move { request.send().await.unwrap().text().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(1), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    stop.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!task.is_finished(), "shutdown must drain an active request");
    release.add_permits(1);
    assert_eq!(pending.await.unwrap(), "drained");
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    drop(access);
    let raw = std::fs::read_to_string(log_path).unwrap();
    let rows: Vec<AccessRecord> = raw
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 4);
    assert!(rows
        .iter()
        .all(|r| r.peer_ip.as_deref() == Some("127.0.0.1")));
    assert!(rows.iter().any(|r| r.status == Some(401)));
    assert!(rows
        .iter()
        .any(|r| r.client_ip.as_deref() == Some("198.51.100.44")));
    assert!(!raw.contains("listener-test"));
}
#[tokio::test]
async fn plain_listener_remains_available_without_tls_configuration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, signal) = tokio::sync::oneshot::channel();
    let router = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
    let task = tokio::spawn(crate::server::serve(listener, router, None, async {
        let _ = signal.await;
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert_eq!(
        client
            .get(format!("http://{address}/health"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn deployment_tls_is_top_level_and_invalid_material_fails_preparation() {
    use crate::deployment::{DeploymentConfig, MultiConfig};
    let root = tempfile::tempdir().unwrap();
    let tls = listener_tls_config(root.path());
    let mut region = regional_config(crate::region::Region::Jp);
    region.tls = Some(tls.clone());
    let mut deployment = MultiConfig {
        logging: None,
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: None,
        access_log: None,
        regions: BTreeMap::from([("jp".into(), region)]),
    };
    assert!(DeploymentConfig::Multi(Box::new(deployment.clone()))
        .validate()
        .is_err());
    deployment.regions.get_mut("jp").unwrap().tls = None;
    deployment.tls = Some(tls.clone());
    assert!(DeploymentConfig::Multi(Box::new(deployment.clone()))
        .prepare()
        .unwrap()
        .tls
        .is_some());
    std::fs::remove_file(tls.private_key_file).unwrap();
    assert!(DeploymentConfig::Multi(Box::new(deployment))
        .prepare()
        .is_err());
}

fn access_log_config(path: std::path::PathBuf) -> crate::access_log::Config {
    crate::access_log::Config {
        output: crate::access_log::Output::File {
            path,
            rotation: crate::access_log::Rotation::Never,
            max_files: 7,
        },
        trusted_proxies: vec![
            "127.0.0.0/8".into(),
            "10.0.0.0/8".into(),
            "::1/128".into(),
            "fd00::/8".into(),
        ],
        ..Default::default()
    }
}
#[derive(serde::Deserialize)]
struct AccessRecord {
    request_id: String,
    method: String,
    route: String,
    peer_ip: Option<String>,
    client_ip: Option<String>,
    status: Option<u16>,
    outcome: String,
}
#[tokio::test]
async fn access_log_redacts_identifiers_and_resolves_only_trusted_forwarding_chains() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("access.log");
    let log = crate::access_log::AccessLog::new(access_log_config(path.clone())).unwrap();
    let app = log.wrap(
        axum::Router::new()
            .route(
                "/players/{id}",
                axum::routing::get(
                    |axum::Extension(ip): axum::Extension<crate::access_log::ClientIp>| async move {
                        ip.0.map(|p| p.to_string()).unwrap_or_default()
                    },
                ),
            )
            .route(
                "/denied",
                axum::routing::get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
            ),
    );
    let mut ids = vec![];
    for (peer, forwarded, expected) in [
        ("203.0.113.9:123", "198.51.100.1", "203.0.113.9"),
        ("127.0.0.1:123", "198.51.100.1, 10.0.0.2", "198.51.100.1"),
        (
            "127.0.0.1:123",
            "192.0.2.66, 203.0.113.7, 10.0.0.2",
            "203.0.113.7",
        ),
        ("127.0.0.1:123", "unknown, 10.0.0.2", "127.0.0.1"),
        ("[::ffff:127.0.0.1]:123", "198.51.100.1", "198.51.100.1"),
        ("[::1]:123", "2001:db8::2, fd00::1", "2001:db8::2"),
    ] {
        let mut request = axum::http::Request::get("/players/private-player-id?token=query-secret")
            .header("authorization", "Bearer secret-token")
            .header("cookie", "secret-cookie")
            .header("x-request-id", "attacker-request-id")
            .header("x-forwarded-for", forwarded)
            .body(axum::body::Body::from("secret-body"))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        ids.push(
            response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned(),
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            expected.as_bytes()
        );
    }
    // Ambiguous repeated forwarding headers are ignored rather than concatenated.
    let mut request = axum::http::Request::get("/denied")
        .header("x-forwarded-for", "198.51.100.1")
        .body(axum::body::Body::empty())
        .unwrap();
    request
        .headers_mut()
        .append("x-forwarded-for", "192.0.2.1".parse().unwrap());
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:123".parse::<std::net::SocketAddr>().unwrap(),
    ));
    assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 401);
    let request = axum::http::Request::get("/unmatched-secret?token=query-secret")
        .header("x-forwarded-for", "198.51.100.1")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 404);
    drop(app);
    drop(log); // Worker guard flushes queued lines.
    let raw = std::fs::read_to_string(path).unwrap();
    for secret in [
        "private-player-id",
        "query-secret",
        "secret-token",
        "secret-cookie",
        "secret-body",
        "attacker-request-id",
        "unmatched-secret",
    ] {
        assert!(!raw.contains(secret));
    }
    let rows: Vec<AccessRecord> = raw
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 8);
    for (i, row) in rows[..6].iter().enumerate() {
        assert_eq!(row.request_id, ids[i]);
        assert!(uuid::Uuid::parse_str(&row.request_id).is_ok());
        assert_eq!(row.route, "/players/{id}");
        assert_eq!(row.method, "GET");
        assert_eq!(row.status, Some(200));
        assert_eq!(row.outcome, "response");
    }
    assert_eq!(rows[0].client_ip.as_deref(), Some("203.0.113.9"));
    assert_eq!(rows[2].client_ip.as_deref(), Some("203.0.113.7"));
    assert_eq!(rows[4].peer_ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(rows[6].client_ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(rows[7].route, "<unmatched>");
    assert!(rows[7].client_ip.is_none());
}
#[tokio::test]
async fn cancelled_access_request_is_recorded_and_text_files_append() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("access.log");
    std::fs::write(&path, "previous\n").unwrap();
    let mut config = access_log_config(path.clone());
    config.format = crate::access_log::Format::Text;
    let log = crate::access_log::AccessLog::new(config).unwrap();
    let entered = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let signal = entered.clone();
    let app = log.wrap(axum::Router::new().route(
        "/pending",
        axum::routing::get(move || {
            let signal = signal.clone();
            async move {
                signal.add_permits(1);
                std::future::pending::<()>().await;
                "unreachable"
            }
        }),
    ));
    let task = tokio::spawn(
        app.oneshot(
            axum::http::Request::get("/pending?ignored-secret")
                .body(axum::body::Body::empty())
                .unwrap(),
        ),
    );
    tokio::time::timeout(Duration::from_secs(1), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(log);
    let lines = std::fs::read_to_string(path).unwrap();
    assert!(lines.starts_with("previous\n"));
    assert_eq!(lines.lines().count(), 2);
    assert!(lines.contains("outcome=cancelled"));
    assert!(lines.contains("status=-"));
    assert!(!lines.contains("ignored-secret"));
}
#[test]
fn access_log_configuration_rejects_bad_trust_headers_and_unbounded_queues() {
    for text in [
        "queue_capacity: 0",
        "queue_capacity: 65537",
        "trusted_proxies: [not-a-cidr]",
        "proxy_header: authorization",
        "proxy_header: 'invalid header'",
        "output: {type: file, path: /, max_files: 7}",
        "output: {type: file, path: access.log, max_files: 0}",
    ] {
        let config: crate::access_log::Config = yaml_serde::from_str(text).unwrap();
        assert!(config.validate().is_err(), "{text}");
    }
    assert!(yaml_serde::from_str::<crate::access_log::Config>(
        "output: {type: stdout, path: ignored}"
    )
    .is_err());
}

#[tokio::test]
async fn master_network_retries_only_transient_downloads_and_keeps_integrity_failures_terminal() {
    use crate::master_update::MasterUpdater;
    for (status, retry) in [
        (429, true),
        (503, true),
        (403, false),
        (407, false),
        (302, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut failure = cdn_reply(vec![]);
        failure.http_status = status;
        let replies = if retry {
            vec![
                failure,
                cdn_reply(remote_master_manifest()),
                cdn_reply(master_fixture().2.to_vec()),
            ]
        } else {
            vec![failure]
        };
        let cdn = cdn_fixture(replies).await;
        let game = fixture(vec![Reply::version(), Reply::version()]).await;
        let mut cfg = remote_master_config(&cdn, root.path());
        let network = &mut cfg.master_update.as_mut().unwrap().network;
        network.attempts = 3;
        network.retry_delay_ms = 1;
        let result = MasterUpdater::new(&cfg, client(&game, cfg.clone()))
            .unwrap()
            .update_once()
            .await;
        assert_eq!(result.is_ok(), retry, "status {status}");
        assert_eq!(
            cdn.received.lock().unwrap().len(),
            if retry { 3 } else { 1 }
        );
        assert_eq!(root.path().join("CURRENT").exists(), retry);
    }
    let root = tempfile::tempdir().unwrap();
    let mut bad = master_fixture().2.to_vec();
    bad[33] ^= 1;
    let cdn = cdn_fixture(vec![cdn_reply(remote_master_manifest()), cdn_reply(bad)]).await;
    let game = fixture(vec![Reply::version()]).await;
    let mut cfg = remote_master_config(&cdn, root.path());
    cfg.master_update.as_mut().unwrap().network.attempts = 3;
    assert!(MasterUpdater::new(&cfg, client(&game, cfg.clone()))
        .unwrap()
        .update_once()
        .await
        .is_err());
    assert_eq!(cdn.received.lock().unwrap().len(), 2);
    assert!(!root.path().join("CURRENT").exists());
}

#[tokio::test]
async fn master_proxy_is_independent_of_game_transport_and_never_falls_back() {
    use crate::master_update::MasterUpdater;
    for reject in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let direct = cdn_fixture(vec![]).await;
        let replies = if reject {
            let mut reply = cdn_reply(vec![]);
            reply.http_status = 407;
            vec![reply]
        } else {
            vec![
                cdn_reply(remote_master_manifest()),
                cdn_reply(master_fixture().2.to_vec()),
            ]
        };
        // An HTTP forward-proxy fixture answers absolute-form requests itself.
        let proxy = cdn_fixture(replies).await;
        let game = fixture(vec![Reply::version(), Reply::version()]).await;
        let mut cfg = remote_master_config(&direct, root.path());
        let url_env = format!("MASTER_PROXY_URL_{}", uuid::Uuid::new_v4().simple());
        let auth_env = format!("MASTER_PROXY_AUTH_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&url_env, &proxy.url);
        std::env::set_var(&auth_env, "Bearer proxy-only");
        let network = &mut cfg.master_update.as_mut().unwrap().network;
        network.proxy_url_env = Some(url_env.clone());
        network.proxy_authorization_env = Some(auth_env.clone());
        network.attempts = 3;
        let result = MasterUpdater::new(&cfg, client(&game, cfg.clone()))
            .unwrap()
            .update_once()
            .await;
        assert_eq!(result.is_ok(), !reject);
        assert!(direct.received.lock().unwrap().is_empty());
        for (_, headers, _) in proxy.received.lock().unwrap().iter() {
            assert_eq!(headers["proxy-authorization"], "Bearer proxy-only");
            assert_eq!(
                headers["authorization"],
                "Basic Zml4dHVyZS11c2VyOmZpeHR1cmUtY2RuLXNlY3JldA=="
            );
            assert!(
                !headers.contains_key("x-player-id")
                    && !headers.contains_key("x-player-credential")
            );
        }
        for (_, headers, _) in game.received.lock().unwrap().iter() {
            assert!(!headers.contains_key("proxy-authorization"));
        }
        assert_eq!(
            proxy.received.lock().unwrap().len(),
            if reject { 1 } else { 2 }
        );
        std::env::remove_var(url_env);
        std::env::remove_var(auth_env);
    }
}

#[tokio::test]
async fn master_request_timeout_retries_but_whole_update_deadline_bounds_backoff() {
    use crate::master_update::{MasterUpdater, UpdateError};
    for whole_deadline in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut delayed = cdn_reply(remote_master_manifest());
        delayed.delay = Duration::from_secs(2);
        let mut unavailable = cdn_reply(vec![]);
        unavailable.http_status = 503;
        let cdn = cdn_fixture(if whole_deadline {
            vec![unavailable]
        } else {
            vec![
                delayed,
                cdn_reply(remote_master_manifest()),
                cdn_reply(master_fixture().2.to_vec()),
            ]
        })
        .await;
        let game = fixture(vec![Reply::version(), Reply::version()]).await;
        let mut cfg = remote_master_config(&cdn, root.path());
        let network = &mut cfg.master_update.as_mut().unwrap().network;
        network.request_timeout_ms = 100;
        network.attempts = 3;
        network.retry_delay_ms = 1;
        if whole_deadline {
            network.update_timeout_seconds = 1;
            network.retry_delay_ms = 5000;
        }
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            MasterUpdater::new(&cfg, client(&game, cfg.clone()))
                .unwrap()
                .update_once(),
        )
        .await
        .unwrap();
        if whole_deadline {
            assert!(matches!(result, Err(UpdateError::Timeout)));
        } else {
            assert!(result.is_ok());
        }
        assert_eq!(
            cdn.received.lock().unwrap().len(),
            if whole_deadline { 1 } else { 3 }
        );
        assert_eq!(root.path().join("CURRENT").exists(), !whole_deadline);
        assert!(crate::master::WriterLock::acquire(root.path()).is_ok());
    }
}

#[test]
fn master_network_configuration_is_bounded_and_old_yaml_keeps_defaults() {
    let old: crate::config::MasterUpdateConfig = yaml_serde::from_str(
        "username_env: U\nkey_hex_env: K\niv_hex_env: I\ninterval_seconds: 60",
    )
    .unwrap();
    assert_eq!(old.network.attempts, 1);
    assert_eq!(old.network.update_timeout_seconds, 600);
    assert!(old.network.proxy_url_env.is_none());
    for yaml in [
        "attempts: 0",
        "attempts: 9",
        "request_timeout_ms: 99",
        "connect_timeout_ms: 300001",
        "update_timeout_seconds: 0",
        "retry_delay_ms: 0",
        "max_retry_delay_ms: 1",
        "proxy_authorization_env: AUTH",
        "proxy_url_env: 'bad name'",
    ] {
        let network: crate::master_update::Network = yaml_serde::from_str(yaml).unwrap();
        assert!(network.validate().is_err(), "{yaml}");
    }
    assert!(yaml_serde::from_str::<crate::master_update::Network>("password: ignored").is_err());
}

#[tokio::test]
async fn master_proxy_and_origin_tls_reject_untrusted_certificates() {
    use crate::master_update::MasterUpdater;
    for proxy_tls in [false, true] {
        let (url, rejected) = untrusted_tls_fixture().await;
        let root = tempfile::tempdir().unwrap();
        let direct = cdn_fixture(vec![]).await;
        let game = fixture(vec![Reply::version()]).await;
        let mut cfg = remote_master_config(&direct, root.path());
        let tunnel = tunnel_proxy(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), true, Duration::ZERO).await;
        let reference = cfg.cdn_credential_env.values().next().unwrap().clone();
        if !proxy_tls {
            cfg.default_cdn_root = url.clone();
            cfg.cdn_credential_env = BTreeMap::from([(url.clone(), reference)]);
        }
        let url_env = format!("MASTER_TLS_PROXY_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&url_env, if proxy_tls { &url } else { &tunnel.url });
        cfg.master_update.as_mut().unwrap().network.proxy_url_env = Some(url_env.clone());
        assert!(MasterUpdater::new(&cfg, client(&game, cfg.clone()))
            .unwrap()
            .update_once()
            .await
            .is_err());
        assert!(tokio::time::timeout(Duration::from_secs(2), rejected)
            .await
            .unwrap()
            .unwrap());
        assert!(direct.received.lock().unwrap().is_empty());
        for request in tunnel.seen.lock().unwrap().iter() {
            assert!(request.starts_with("CONNECT "));
            assert!(!request.to_lowercase().contains("authorization:"));
        }
        std::env::remove_var(url_env);
    }
}

#[tokio::test]
async fn master_update_deadline_includes_waiting_for_another_update() {
    use crate::master_update::{MasterUpdater, UpdateError};
    let root = tempfile::tempdir().unwrap();
    let mut delayed = cdn_reply(remote_master_manifest());
    delayed.delay = Duration::from_secs(3);
    let cdn = cdn_fixture(vec![delayed.clone(), delayed]).await;
    let game = fixture(vec![Reply::version(), Reply::version()]).await;
    let mut cfg = remote_master_config(&cdn, root.path());
    cfg.master_update
        .as_mut()
        .unwrap()
        .network
        .update_timeout_seconds = 1;
    let updater = MasterUpdater::new(&cfg, client(&game, cfg.clone())).unwrap();
    let first = updater.clone();
    let first = tokio::spawn(async move { first.update_once().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while cdn.received.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let second = tokio::time::timeout(Duration::from_millis(1500), updater.update_once())
        .await
        .unwrap();
    assert!(matches!(second, Err(UpdateError::Timeout)));
    assert!(matches!(first.await.unwrap(), Err(UpdateError::Timeout)));
    assert!(!root.path().join("CURRENT").exists());
    assert!(crate::master::WriterLock::acquire(root.path()).is_ok());
}

#[tokio::test]
async fn master_proxy_invalid_secrets_fail_before_any_network_request() {
    use crate::master_update::MasterUpdater;
    let root = tempfile::tempdir().unwrap();
    let cdn = cdn_fixture(vec![]).await;
    let game = fixture(vec![]).await;
    let mut cfg = remote_master_config(&cdn, root.path());
    let name = format!("MASTER_INVALID_PROXY_{}", uuid::Uuid::new_v4().simple());
    cfg.master_update.as_mut().unwrap().network.proxy_url_env = Some(name.clone());
    for value in [
        "http://user:private-password@localhost",
        "https://localhost/path",
        "socks5://localhost",
        "https://localhost/?private-token",
        "https://localhost/#private-token",
    ] {
        std::env::set_var(&name, value);
        let error = MasterUpdater::new(&cfg, client(&game, cfg.clone()))
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, "Master updater configuration is invalid");
    }
    std::env::remove_var(name);
    assert!(MasterUpdater::new(&cfg, client(&game, cfg.clone())).is_err());
    assert!(cdn.received.lock().unwrap().is_empty());
    assert!(game.received.lock().unwrap().is_empty());
}

#[test]
fn application_logging_filters_levels_fields_and_dependency_targets_and_flushes_on_drop() {
    use crate::{
        access_log::{Format, Output, Rotation},
        application_log,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("application.log");
    for _ in 0..2 {
        let config = application_log::Config {
            level: application_log::Level::Info,
            format: Format::Json,
            output: Output::File {
                path: path.clone(),
                rotation: Rotation::Never,
                max_files: 7,
            },
            queue_capacity: 128,
        };
        let (subscriber, guard) = config.subscriber().unwrap();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("debug-must-not-appear");
            tracing::info!(target: "reqwest::connect", "dependency-secret-must-not-appear");
            tracing::info!(
                authorization = "credential-must-not-appear",
                body = "body-must-not-appear",
                completed = 7_u64,
                stage = "verify\nforged-line",
                "safe application event"
            );
            tracing::warn!(error_code = "synthetic_failure", "safe failure event");
        });
        drop(guard);
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["fields"]["completed"].as_u64(), Some(7));
    assert_eq!(
        rows[0]["fields"]["stage"].as_str(),
        Some("verify\nforged-line")
    );
    assert_eq!(rows[1]["level"].as_str(), Some("WARN"));
    assert!(!text.contains("must-not-appear"));
    assert!(!text.contains("\nforged-line"));
    assert!(rows
        .iter()
        .all(|row| row["queue_dropped_records"].as_u64() == Some(0)));
}

#[test]
fn application_logging_off_text_bounds_and_invalid_output_are_enforced() {
    use crate::{
        access_log::{Format, Output, Rotation},
        application_log,
    };
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events.log");
    let mut config = application_log::Config {
        level: application_log::Level::Off,
        format: Format::Text,
        output: Output::File {
            path: path.clone(),
            rotation: Rotation::Never,
            max_files: 3,
        },
        queue_capacity: 32,
    };
    let (subscriber, guard) = config.subscriber().unwrap();
    tracing::subscriber::with_default(subscriber, || {
        tracing::error!("off-must-not-appear");
    });
    drop(guard);
    assert!(std::fs::read_to_string(&path).unwrap().is_empty());
    config.level = application_log::Level::Trace;
    let (subscriber, guard) = config.subscriber().unwrap();
    tracing::subscriber::with_default(subscriber, || {
        tracing::trace!(stage = "雪".repeat(2000), "bounded application event");
    });
    drop(guard);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("TRACE"));
    assert!(text.len() < 1400);
    config.queue_capacity = 0;
    assert!(config.validate().is_err());
    config.queue_capacity = 32;
    let blocked = root.path().join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    config.output = Output::File {
        path: blocked.join("must-not-appear.log"),
        rotation: Rotation::Never,
        max_files: 3,
    };
    let error = config.subscriber().err().unwrap().to_string();
    assert!(!error.contains("must-not-appear"));
    for yaml in [
        "level: verbose",
        "queue_capacity: 65537",
        "output: {type: stderr, path: ignored}",
        "output: {type: file, path: log, max_files: 0}",
    ] {
        assert!(!yaml_serde::from_str::<application_log::Config>(yaml)
            .is_ok_and(|c| c.validate().is_ok()));
    }
}

#[tokio::test]
async fn asset_job_transport_validates_identity_auth_and_bounded_responses() {
    use crate::asset_jobs::{Client, Error, Operation, Request, Status};
    use axum::{extract::State, http::HeaderMap, response::IntoResponse, routing::any, Router};
    use sha2::{Digest, Sha256};
    let id = uuid::Uuid::new_v4().to_string();
    let request = Request {
        region: crate::region::Region::Jp,
        profile: "full".into(),
        operation: Operation::Update,
    };
    let queued = serde_json::json!({"id":id,"request":request,"status":"queued","idempotency_sha256":format!("{:x}",Sha256::digest(b"key-1"))});
    let state = std::sync::Arc::new(std::sync::Mutex::new((
        axum::http::StatusCode::ACCEPTED,
        queued.clone(),
        0u64,
    )));
    let app = Router::new()
        .route(
            "/{*path}",
            any(
                |State(state): State<
                    std::sync::Arc<
                        std::sync::Mutex<(axum::http::StatusCode, serde_json::Value, u64)>,
                    >,
                >,
                 method: axum::http::Method,
                 headers: HeaderMap,
                 body: axum::body::Bytes| async move {
                    assert_eq!(headers["authorization"], "Bearer fixture-updater-only");
                    assert!(!headers.contains_key("proxy-authorization"));
                    if method == axum::http::Method::POST {
                        assert_eq!(headers["idempotency-key"], "key-1");
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["region"],
                            "jp"
                        );
                    }
                    let (status, value, delay) = state.lock().unwrap().clone();
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    (
                        status,
                        [("location", "http://127.0.0.1:1/forbidden")],
                        axum::Json(value),
                    )
                        .into_response()
                },
            ),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new(&root, "fixture-updater-only", true, 200).unwrap();
    let job = client.submit(&request, "key-1").await.unwrap();
    assert_eq!(job.id, id);
    assert_eq!(job.status, Status::Queued);
    state.lock().unwrap().0 = axum::http::StatusCode::OK;
    assert_eq!(client.get(&id, &request).await.unwrap().id, id);
    let mut completed = queued.clone();
    completed["status"] = "completed".into();
    completed["outcome"] = serde_json::json!({"verification":{"region":"jp","platform":"iOS","environment":"production","resource_version":"version-1","platform_hash":"hash-1","catalog_sha256":"a".repeat(64),"full_catalog":true,"catalog_verified":true},"export":{"full_export":true,"retained":false,"files":2,"bytes":16},"publication_id":uuid::Uuid::new_v4().to_string()});
    state.lock().unwrap().1 = completed.clone();
    assert!(
        client
            .get(&id, &request)
            .await
            .unwrap()
            .outcome
            .unwrap()
            .verification
            .full_catalog
    );
    for bad in [
        {
            let mut v = completed.clone();
            v["outcome"]["verification"]["region"] = "en".into();
            v
        },
        {
            let mut v = completed.clone();
            v["outcome"]["verification"]["catalog_sha256"] = "not-sha".into();
            v
        },
        {
            let mut v = completed.clone();
            v["status"] = "failed".into();
            v
        },
        {
            let mut v = queued.clone();
            v["id"] = uuid::Uuid::new_v4().to_string().into();
            v
        },
        {
            let mut v = queued.clone();
            v["request"]["profile"] = "other".into();
            v
        },
        serde_json::json!({"padding":"x".repeat(65537)}),
    ] {
        state.lock().unwrap().1 = bad;
        assert!(matches!(
            client.get(&id, &request).await,
            Err(Error::Protocol)
        ));
    }
    *state.lock().unwrap() = (axum::http::StatusCode::ACCEPTED, queued.clone(), 0);
    state.lock().unwrap().1["idempotency_sha256"] = "b".repeat(64).into();
    assert!(matches!(
        client.submit(&request, "key-1").await,
        Err(Error::Protocol)
    ));
    for status in [302, 401, 404, 409, 429, 503] {
        *state.lock().unwrap() = (
            axum::http::StatusCode::from_u16(status).unwrap(),
            serde_json::json!({"secret":"never-forward-this"}),
            0,
        );
        let error = client.submit(&request, "key-1").await.unwrap_err();
        assert!(matches!(error,Error::Status(code) if code==status));
        assert!(!error.to_string().contains("never-forward"));
    }
    *state.lock().unwrap() = (axum::http::StatusCode::OK, queued, 1000);
    assert!(matches!(
        client.get(&id, &request).await,
        Err(Error::Transport)
    ));
    assert!(matches!(
        client.get("../other", &request).await,
        Err(Error::Config)
    ));
    assert!(matches!(
        client.submit(&request, "bad key").await,
        Err(Error::Config)
    ));
    let mut reserved = request.clone();
    reserved.region = crate::region::Region::Cn;
    assert!(matches!(
        client.submit(&reserved, "key-1").await,
        Err(Error::Config)
    ));
    assert!(Client::new(&root, "token", false, 200).is_err());
    for root in [
        "https://user:pass@example.com",
        "https://example.com/path",
        "https://example.com?token=value",
    ] {
        assert!(Client::new(root, "token", false, 200).is_err());
    }
    assert!(Client::new("https://example.com", "token\nsecret", false, 200).is_err());
    server.abort();
}

#[test]
fn asset_outbox_preserves_ambiguous_delivery_and_terminal_identity_across_restart() {
    use crate::{
        asset_jobs::{Operation, Request},
        asset_outbox::{Error, Identity, Outbox, State},
        region::Region,
    };
    let directory = tempfile::tempdir().unwrap();
    let identity = Identity {
        destination_sha256: "a".repeat(64),
        request: Request {
            region: Region::Jp,
            profile: "full".into(),
            operation: Operation::Update,
        },
        profile_revision: "1".into(),
        environment: "production".into(),
        platform: "iOS".into(),
        resource_version: "version-1".into(),
        platform_hash: "hash-1".into(),
        require_full_catalog: true,
        require_full_export: true,
        require_publication: true,
    };
    let mut store = Outbox::open(directory.path(), 2).unwrap();
    assert!(matches!(
        Outbox::open(directory.path(), 2),
        Err(Error::Locked)
    ));
    let key = store.observe(identity.clone()).unwrap();
    assert_eq!(store.observe(identity.clone()).unwrap(), key);
    let id = uuid::Uuid::new_v4().to_string();
    assert!(store.acknowledge(&key, &id).is_err());
    store.begin_send(&key).unwrap();
    let sending = store.entries()[&key].state.clone();
    drop(store);
    let mut store = Outbox::open(directory.path(), 2).unwrap();
    assert_eq!(store.entries()[&key].state, sending);
    store.begin_send(&key).unwrap();
    assert_eq!(store.entries()[&key].state, sending);
    store.acknowledge(&key, &id).unwrap();
    assert!(store
        .acknowledge(&key, &uuid::Uuid::new_v4().to_string())
        .is_err());
    assert!(store
        .complete(
            &key,
            &uuid::Uuid::new_v4().to_string(),
            &"b".repeat(64),
            None
        )
        .is_err());
    store.complete(&key, &id, &"b".repeat(64), None).unwrap();
    assert!(store.begin_send(&key).is_err());
    drop(store);
    let mut store = Outbox::open(directory.path(), 2).unwrap();
    assert_eq!(store.observe(identity.clone()).unwrap(), key);
    assert!(matches!(
        store.entries()[&key].state,
        State::Completed { .. }
    ));
    let mut revised = identity.clone();
    revised.profile_revision = "2".into();
    let second = store.observe(revised).unwrap();
    assert_ne!(second, key);
    store.fail(&second, "job_pruned").unwrap();
    assert!(store.begin_send(&second).is_err());
    let mut next = identity.clone();
    next.resource_version = "version-2".into();
    assert!(matches!(store.observe(next), Err(Error::Full)));
    for variant in 0..6 {
        let mut other = identity.clone();
        match variant {
            0 => other.request.region = Region::En,
            1 => other.destination_sha256 = "c".repeat(64),
            2 => other.request.profile = "subset".into(),
            3 => other.environment = "review".into(),
            4 => other.platform = "Android".into(),
            _ => other.require_full_export = false,
        }
        assert_ne!(identity.key().unwrap(), other.key().unwrap());
    }
}

#[test]
fn asset_outbox_failed_writes_and_corrupt_state_never_acknowledge_dispatch() {
    use crate::{
        asset_jobs::{Operation, Request},
        asset_outbox::{Error, Identity, Outbox, State},
        region::Region,
    };
    let directory = tempfile::tempdir().unwrap();
    let identity = Identity {
        destination_sha256: "a".repeat(64),
        request: Request {
            region: Region::Jp,
            profile: "full".into(),
            operation: Operation::Update,
        },
        profile_revision: "1".into(),
        environment: "production".into(),
        platform: "iOS".into(),
        resource_version: "version-1".into(),
        platform_hash: "hash-1".into(),
        require_full_catalog: true,
        require_full_export: true,
        require_publication: false,
    };
    let mut store = Outbox::open(directory.path(), 2).unwrap();
    let key = store.observe(identity.clone()).unwrap();
    let path = directory.path().join("outbox.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(store.begin_send(&key), Err(Error::Storage)));
    assert_eq!(store.entries()[&key].state, State::Pending);
    std::fs::remove_dir(&path).unwrap();
    store.begin_send(&key).unwrap();
    drop(store);
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["entries"][&key]["identity"]["resource_version"] = "tampered".into();
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(matches!(
        Outbox::open(directory.path(), 2),
        Err(Error::Storage)
    ));
    let mut invalid = identity;
    invalid.request.region = Region::Cn;
    assert!(matches!(invalid.key(), Err(Error::Invalid)));
}

#[tokio::test]
async fn automatic_asset_dispatch_submits_once_and_reconciles_after_restart() {
    type RemoteState = Arc<std::sync::Mutex<(Option<Value>, usize, bool)>>;
    use crate::asset_dispatch::{Config as DispatchConfig, Target, Worker};
    use axum::{extract::State, http::HeaderMap, response::IntoResponse, routing::any, Router};
    use sha2::{Digest, Sha256};
    let directory = tempfile::tempdir().unwrap();
    let remote_state = Arc::new(std::sync::Mutex::new((
        None::<serde_json::Value>,
        0usize,
        false,
    )));
    let app=Router::new().route("/{*path}",any(|State(state):State<RemoteState>,method:axum::http::Method,headers:HeaderMap,body:axum::body::Bytes|async move{
        assert_eq!(headers["authorization"],"Bearer dispatch-only-token");
        let mut state=state.lock().unwrap();
        if method==axum::http::Method::POST {
            state.1+=1;
            let request:Value=serde_json::from_slice(&body).unwrap();
            let key=headers["idempotency-key"].as_bytes();
            state.0=Some(json!({"id":uuid::Uuid::new_v4().to_string(),"request":request,"status":"queued","idempotency_sha256":format!("{:x}",Sha256::digest(key))}));
            return (axum::http::StatusCode::ACCEPTED,axum::Json(state.0.clone().unwrap())).into_response();
        }
        let mut job=state.0.clone().unwrap();
        if state.2 {
            job["status"]="completed".into();
            job["outcome"]=json!({"verification":{"region":"jp","environment":"release","platform":"iOS","resource_version":"r1","platform_hash":"hash1","catalog_sha256":"a".repeat(64),"catalog_verified":true,"full_catalog":true},"export":{"full_export":true,"retained":true,"files":1,"bytes":10},"publication_id":null});
        }
        (axum::http::StatusCode::OK,axum::Json(job)).into_response()
    })).with_state(remote_state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let remote = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let game = fixture(vec![Reply::version()
        .header("x-asset-version", r#"{"version":"r1","iOS":"hash1"}"#)
        .header("x-sirius-cred", "fixture-cdn-secret")])
    .await;
    let mut cfg = config();
    let token = format!("SIRIUS_DISPATCH_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&token, "dispatch-only-token");
    cfg.asset_dispatch = Some(DispatchConfig {
        state_directory: directory.path().join("outbox"),
        interval_seconds: 10,
        request_timeout_ms: 1000,
        history_capacity: 100,
        targets: vec![Target {
            origin,
            token_env: token.clone(),
            allow_http: true,
            profile: "full".into(),
            profile_revision: "1".into(),
            require_full_catalog: true,
            require_full_export: true,
            require_publication: false,
        }],
    });
    let gc = client(&game, cfg.clone());
    let worker = Worker::new(&cfg, gc.clone()).unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(worker.run(rx));
    let path = directory.path().join("outbox/outbox.json");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            if v["entries"]
                .as_object()
                .unwrap()
                .values()
                .any(|v| v["state"]["state"] == "submitted")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(remote_state.lock().unwrap().1, 1);
    let snapshot = gc.snapshot().await.unwrap();
    assert_eq!(snapshot["stale"], false);
    remote_state.lock().unwrap().2 = true;
    let mut restarted = Worker::new(&cfg, gc.clone()).unwrap();
    restarted.reconcile().await.unwrap();
    restarted.reconcile().await.unwrap();
    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        value["entries"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["state"]["state"],
        "completed"
    );
    assert_eq!(remote_state.lock().unwrap().1, 1);
    // A delayed job that processed an older catalog must not complete the new identity.
    let newer = crate::resources::ResourceSnapshot {
        schema_version: 2,
        region: crate::region::Region::Jp,
        environment: "release".into(),
        platform: "iOS",
        client_version: "1.0.3".into(),
        protocol_version: "1.0.3".into(),
        master_version: None,
        resource_version: "r2".into(),
        platform_hash: "hash2".into(),
        effective_cdn_root: String::new(),
        credential_ref: String::new(),
        observed_at: chrono::Utc::now(),
        source: "remote",
    };
    restarted.observe(&newer).unwrap();
    restarted.reconcile().await.unwrap();
    restarted.reconcile().await.unwrap();
    restarted.observe(&newer).unwrap();
    restarted.reconcile().await.unwrap();
    assert_eq!(remote_state.lock().unwrap().1, 2);
    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(value["entries"]
        .as_object()
        .unwrap()
        .values()
        .any(|v| v["state"]["code"] == "outcome_mismatch"));
    drop(restarted);
    // Crash after persisting sending but before acknowledgement is not a safe replay.
    let mut outbox = crate::asset_outbox::Outbox::open(path.parent().unwrap(), 100).unwrap();
    let mut identity = outbox.entries().values().next().unwrap().identity.clone();
    identity.profile_revision = "ambiguous".into();
    let ambiguous = outbox.observe(identity).unwrap();
    outbox.begin_send(&ambiguous).unwrap();
    drop(outbox);
    let mut restarted = Worker::new(&cfg, gc.clone()).unwrap();
    restarted.reconcile().await.unwrap();
    assert_eq!(remote_state.lock().unwrap().1, 2);
    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        value["entries"][&ambiguous]["state"]["code"],
        "submission_ambiguous"
    );
    drop(restarted);
    // Adopting a real but unrelated job must never complete this dispatch identity.
    let wrong_id = remote_state.lock().unwrap().0.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut outbox = crate::asset_outbox::Outbox::open(path.parent().unwrap(), 100).unwrap();
    outbox.adopt(&ambiguous, &wrong_id).unwrap();
    drop(outbox);
    let mut restarted = Worker::new(&cfg, gc.clone()).unwrap();
    restarted.reconcile().await.unwrap();
    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        value["entries"][&ambiguous]["state"]["code"],
        "job_identity_mismatch"
    );
    assert_eq!(remote_state.lock().unwrap().1, 2);
    drop(restarted);
    cfg.environment = "review".into();
    assert!(Worker::new(&cfg, gc).is_err());
    remote.abort();
    std::env::remove_var(token);
}

#[test]
fn asset_outbox_adoption_recovers_existing_work_without_resetting_history() {
    use crate::{
        asset_jobs::{Operation, Request},
        asset_outbox::{Identity, Outbox, State},
        region::Region,
    };
    let directory = tempfile::tempdir().unwrap();
    let identity = Identity {
        destination_sha256: "a".repeat(64),
        request: Request {
            region: Region::Jp,
            profile: "full".into(),
            operation: Operation::Update,
        },
        profile_revision: "1".into(),
        environment: "release".into(),
        platform: "iOS".into(),
        resource_version: "r1".into(),
        platform_hash: "h1".into(),
        require_full_catalog: true,
        require_full_export: true,
        require_publication: false,
    };
    let mut store = Outbox::open(directory.path(), 10).unwrap();
    let key = store.observe(identity.clone()).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    assert!(store.adopt(&key, &id).is_err());
    store.begin_send(&key).unwrap();
    store.fail(&key, "submission_ambiguous").unwrap();
    assert!(store.adopt(&key, "../not-a-job").is_err());
    let path = directory.path().join("outbox.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(store.adopt(&key, &id).is_err());
    assert!(matches!(store.entries()[&key].state, State::Failed { .. }));
    std::fs::remove_dir(&path).unwrap();
    store.adopt(&key, &id).unwrap();
    store.adopt(&key, &id).unwrap();
    assert!(store
        .adopt(&key, &uuid::Uuid::new_v4().to_string())
        .is_err());
    drop(store);
    let mut store = Outbox::open(directory.path(), 10).unwrap();
    assert_eq!(
        store.entries()[&key].state,
        State::Submitted { job_id: id.clone() }
    );
    store.complete(&key, &id, &"b".repeat(64), None).unwrap();
    assert!(store.adopt(&key, &id).is_err());
    let mut other = identity;
    other.resource_version = "r2".into();
    let other = store.observe(other).unwrap();
    store.begin_send(&other).unwrap();
    store
        .acknowledge(&other, &uuid::Uuid::new_v4().to_string())
        .unwrap();
    store.fail(&other, "job_failed").unwrap();
    assert!(store.adopt(&other, &id).is_err());
    assert_eq!(store.entries().len(), 2);
}

#[test]
fn asset_outbox_rotates_blocked_jobs_across_batches_and_restarts() {
    use crate::{
        asset_jobs::{Operation, Request},
        asset_outbox::{Error, Identity, Outbox},
        region::Region,
    };
    let directory = tempfile::tempdir().unwrap();
    let identity = Identity {
        destination_sha256: "a".repeat(64),
        request: Request {
            region: Region::Jp,
            profile: "full".into(),
            operation: Operation::Update,
        },
        profile_revision: "1".into(),
        environment: "release".into(),
        platform: "iOS".into(),
        resource_version: "r1".into(),
        platform_hash: "h1".into(),
        require_full_catalog: true,
        require_full_export: true,
        require_publication: false,
    };
    let mut store = Outbox::open(directory.path(), 100).unwrap();
    assert!(store.next_batch(16).unwrap().is_empty());
    assert!(matches!(store.next_batch(0), Err(Error::Invalid)));
    assert!(matches!(store.next_batch(257), Err(Error::Invalid)));
    for i in 0..35 {
        let mut identity = identity.clone();
        identity.resource_version = format!("r{i}");
        let key = store.observe(identity).unwrap();
        store.begin_send(&key).unwrap();
        store
            .acknowledge(&key, &uuid::Uuid::new_v4().to_string())
            .unwrap();
    }
    let first = store.next_batch(16).unwrap();
    assert_eq!(first.len(), 16);
    let expected: Vec<_> = store.entries().keys().cloned().collect();
    assert_eq!(
        first.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        expected[..16]
    );
    drop(store);
    let mut store = Outbox::open(directory.path(), 100).unwrap();
    let second = store.next_batch(16).unwrap();
    assert_eq!(
        second.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        expected[16..32]
    );
    let third = store.next_batch(16).unwrap();
    assert_eq!(third[0].0, expected[32]);
    assert_eq!(third[3].0, expected[0]);
    let visited: std::collections::HashSet<_> = first
        .iter()
        .chain(second.iter())
        .chain(third.iter())
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(visited.len(), 35); // None of the jobs had to complete for later work to advance.
    for key in &expected {
        store.fail(key, "job_failed").unwrap();
    }
    assert!(store.next_batch(16).unwrap().is_empty());
    let mut added = identity;
    added.resource_version = "new".into();
    let key = store.observe(added).unwrap();
    assert_eq!(store.next_batch(16).unwrap()[0].0, key);
    let before = std::fs::read(directory.path().join("outbox.json")).unwrap();
    let path = directory.path().join("outbox.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(store.next_batch(16), Err(Error::Storage)));
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, before).unwrap();
    assert_eq!(store.next_batch(16).unwrap().len(), 1);
}

#[tokio::test]
async fn stale_cache_refresh_is_background_coalesced_and_keeps_public_fields() {
    let mut delayed = ranking_reply(2);
    delayed.delay = Duration::from_millis(300);
    let f = fixture(vec![Reply::version(), ranking_reply(1), delayed]).await;
    let mut cfg = account_config();
    cfg.response_cache = memory_cache(200);
    if let crate::response_cache::Config::Memory {
        stale_while_revalidate_ms,
        ..
    } = &mut cfg.response_cache
    {
        *stale_while_revalidate_ms = 1000;
    }
    let c = client(&f, cfg);
    let route = crate::client::MUSIC_RANKING;
    assert_eq!(
        c.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        1
    );
    tokio::time::sleep(Duration::from_millis(220)).await;
    let mut calls = Vec::new();
    for _ in 0..12 {
        let c = c.clone();
        calls.push(tokio::spawn(async move {
            c.call(route, json!({"musicId":"1"})).await.unwrap()
        }));
    }
    tokio::time::timeout(Duration::from_millis(200), async {
        for call in calls {
            let value = call.await.unwrap();
            assert_eq!(value["players"][0]["score"], 1);
            assert!(value.get("myRank").is_none());
            assert!(value.get("myScore").is_none());
        }
    })
    .await
    .expect("stale callers must not wait for the session-locked refresh");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value = c.call(route, json!({"musicId":"1"})).await.unwrap();
            if value["players"][0]["score"] == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.received.lock().unwrap().len(),
        3,
        "one bootstrap, one fill, one refresh"
    );
}

#[tokio::test]
async fn stale_cache_has_a_hard_expiry_and_failure_does_not_extend_it() {
    use crate::response_cache::{Cache, Config};
    let mut cfg = memory_cache(20);
    if let Config::Memory {
        stale_while_revalidate_ms,
        ..
    } = &mut cfg
    {
        *stale_while_revalidate_ms = 80;
    }
    let cache = Cache::new(cfg).unwrap();
    cache.put("expiry".into(), &json!({"value": 1})).await;
    assert!(!cache.get_with_state("expiry").await.unwrap().stale);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(cache.get("expiry").await.is_none());
    assert!(cache.get_with_state("expiry").await.unwrap().stale);
    tokio::time::sleep(Duration::from_millis(90)).await;
    assert!(cache.get_with_state("expiry").await.is_none());

    let f = fixture(vec![
        Reply::version(),
        ranking_reply(1),
        unavailable_reply(),
        ranking_reply(3),
    ])
    .await;
    let mut cfg = account_config();
    cfg.response_cache = memory_cache(20);
    if let Config::Memory {
        stale_while_revalidate_ms,
        ..
    } = &mut cfg.response_cache
    {
        *stale_while_revalidate_ms = 100;
    }
    let c = client(&f, cfg);
    let route = crate::client::MUSIC_RANKING;
    c.call(route, json!({"musicId":"1"})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        c.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        1
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        c.call(route, json!({"musicId":"1"})).await.unwrap()["players"][0]["score"],
        3
    );
    assert_eq!(f.received.lock().unwrap().len(), 4);
}

fn peer_request(identity: crate::peer::Identity, operation: Value) -> Value {
    json!({"request_id":uuid::Uuid::new_v4().to_string(),"identity":identity,"operation":operation})
}
async fn peer_send(app: axum::Router, token: &str, payload: Value) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/internal/v1/peer/query")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&payload).unwrap(),
            ))
            .unwrap(),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn peer_queries_execute_local_rpc_and_return_typed_game_failure() {
    let mut failure = Reply::version();
    failure
        .trailers
        .insert("grpc-status", "14".parse().unwrap());
    failure
        .trailers
        .insert("grpc-message", "SECRET_MUST_NOT_ESCAPE".parse().unwrap());
    let f = fixture(vec![Reply::version(), failure]).await;
    let c = client(&f, config());
    let app = crate::peer::router(c.clone(), "/internal/v1/peer", "peer".into());
    let request = peer_request(c.peer_identity().unwrap(), json!({"type":"version"}));
    let response = peer_send(app.clone(), "peer", request.clone()).await;
    assert_eq!(response.status(), 200);
    let reply = body(response).await;
    assert_eq!(reply["request_id"], request["request_id"]);
    assert_eq!(reply["identity"], request["identity"]);
    assert_eq!(reply["outcome"]["data"]["version"], "master-fixture");
    let response = body(peer_send(app, "peer", request).await).await;
    assert_eq!(
        response["outcome"],
        json!({"status":"failure","kind":{"type":"game","grpc_status":14}})
    );
    assert!(!response.to_string().contains("SECRET"));
    let seen = f.received.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (route, headers, _) in seen.iter() {
        assert_eq!(route, VERSION);
        assert!(headers.get("authorization").is_none());
    }
}
#[tokio::test]
async fn peer_auth_identity_allowlist_and_limits_reject_before_game_calls() {
    let f = fixture(vec![]).await;
    let c = client(&f, config());
    let app = api::router(c.clone(), "api".into(), "internal".into()).merge(crate::peer::router(
        c.clone(),
        "/internal/v1/peer",
        "peer".into(),
    ));
    let request = peer_request(c.peer_identity().unwrap(), json!({"type":"version"}));
    for token in ["", "api", "internal"] {
        assert_eq!(
            peer_send(app.clone(), token, request.clone())
                .await
                .status(),
            401
        );
    }
    let admin = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/v1/protocol/reload")
                .header("authorization", "Bearer peer")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin.status(), 401);
    for (key, value) in [
        ("region", json!("tw")),
        ("region", json!("cn")),
        ("environment", json!("review")),
        ("platform", json!("Android")),
        ("client_version", json!("9.0.0")),
        ("protocol_sha256", json!("0".repeat(64))),
        ("contract_version", json!(2)),
    ] {
        let mut invalid = request.clone();
        invalid["identity"][key] = value;
        let response = body(peer_send(app.clone(), "peer", invalid).await).await;
        assert_eq!(response["outcome"]["kind"]["type"], "identity_mismatch");
    }
    for operation in [
        json!({"type":"whoami"}),
        json!({"type":"player_data"}),
        json!({"type":"rpc","path":VERSION}),
        json!({"type":"version","credentials":"SECRET"}),
        json!({"type":"event_ranking","event_id":1,"ranks":[1,1]}),
        json!({"type":"profile","profile_id":0}),
        json!({"type":"event_deck","event_id":1,"player_id":"../bad"}),
        json!({"type":"announcements","tab":3}),
    ] {
        let invalid = peer_request(c.peer_identity().unwrap(), operation);
        assert!(peer_send(app.clone(), "peer", invalid)
            .await
            .status()
            .is_client_error());
    }
    let unsupported = peer_request(c.peer_identity().unwrap(), json!({"type":"servers"}));
    assert_eq!(
        body(peer_send(app.clone(), "peer", unsupported).await).await["outcome"]["kind"]["type"],
        "unsupported_operation"
    );
    let mut oversized = request.clone();
    oversized["operation"]["padding"] = json!("X".repeat(17000));
    assert_eq!(peer_send(app, "peer", oversized).await.status(), 413);
    assert!(f.received.lock().unwrap().is_empty());
}
#[tokio::test]
async fn peer_rejects_stale_protocol_after_reload_before_dispatch() {
    let directory = copy_protocol_bundle();
    let mut cfg = config();
    cfg.protocol_directory = directory.path().into();
    let f = fixture(vec![]).await;
    let c = client(&f, cfg);
    let old = c.peer_identity().unwrap();
    edit_version_proto(
        directory.path(),
        "string version = 1;",
        "string version = 1;\n  string extra = 2;",
    );
    c.reload_protocol().await.unwrap();
    // This calls the dispatch guard directly: a router's earlier identity check
    // alone is insufficient if a reload activates while admission is queued.
    assert!(matches!(
        c.call_peer(VERSION, json!({}), &old.protocol_sha256).await,
        Err(AppError::PeerIdentityMismatch)
    ));
    assert!(f.received.lock().unwrap().is_empty());
}
#[tokio::test]
async fn peer_deployment_is_opt_in_and_has_independent_region_credentials() {
    use crate::{deployment::DeploymentConfig, region::Region};
    let mut cfg = regional_config(Region::Jp);
    let absent = DeploymentConfig::Single(Box::new(cfg.clone()))
        .prepare()
        .unwrap()
        .router;
    let request = peer_request(
        GameClient::new(cfg.clone())
            .unwrap()
            .peer_identity()
            .unwrap(),
        json!({"type":"version"}),
    );
    assert_eq!(
        peer_send(absent, "peer", request.clone()).await.status(),
        404
    );
    let name = format!("SIRIUS_TEST_PEER_{}", uuid::Uuid::new_v4().simple());
    cfg.peer_token_env = Some(name.clone());
    for token in ["public-jp", "internal-jp", "fixture-cdn-secret", " "] {
        std::env::set_var(&name, token);
        assert!(DeploymentConfig::Single(Box::new(cfg.clone()))
            .prepare()
            .is_err());
    }
    std::env::set_var(&name, "dedicated-peer");
    let app = DeploymentConfig::Single(Box::new(cfg.clone()))
        .prepare()
        .unwrap()
        .router;
    assert_eq!(peer_send(app, "internal-jp", request).await.status(), 401);
    let mut tw = regional_config(Region::Tw);
    tw.peer_token_env = Some(name);
    let multi = crate::deployment::MultiConfig {
        logging: None,
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: None,
        access_log: None,
        regions: BTreeMap::from([("jp".into(), cfg), ("tw".into(), tw)]),
    };
    assert!(DeploymentConfig::Multi(Box::new(multi)).prepare().is_err());
}

#[tokio::test]
async fn peer_global_regions_share_schema_but_never_identity_or_capabilities() {
    use crate::region::Region;
    let f = fixture(vec![]).await;
    let c = client(&f, regional_config(Region::Tw));
    let app = crate::peer::router(c.clone(), "/internal/v1/peer", "tw-peer".into());
    for region in [Region::En, Region::Kr] {
        let mut identity = c.peer_identity().unwrap();
        assert_eq!(region.family(), identity.region.family());
        identity.region = region;
        let request = peer_request(identity, json!({"type":"version"}));
        let reply = body(peer_send(app.clone(), "tw-peer", request).await).await;
        assert_eq!(reply["outcome"]["kind"]["type"], "identity_mismatch");
    }
    let request = peer_request(
        c.peer_identity().unwrap(),
        json!({"type":"profile","profile_id":1}),
    );
    let reply = body(peer_send(app, "tw-peer", request).await).await;
    assert_eq!(reply["outcome"]["kind"]["type"], "unsupported_operation");
    assert!(f.received.lock().unwrap().is_empty());
}

async fn peer_http_server(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}
fn outgoing_peer_request() -> crate::peer::Request {
    serde_json::from_value(peer_request(
        GameClient::new(config()).unwrap().peer_identity().unwrap(),
        json!({"type":"version"}),
    ))
    .unwrap()
}
#[tokio::test]
async fn peer_transport_reaches_real_local_executor_with_exact_scope() {
    use crate::{
        peer_transport::{Client, Error, Policy},
        region::Region,
    };
    let f = fixture(vec![Reply::version()]).await;
    let game = client(&f, config());
    let request: crate::peer::Request = serde_json::from_value(peer_request(
        game.peer_identity().unwrap(),
        json!({"type":"version"}),
    ))
    .unwrap();
    let app = crate::peer::router(game, "/internal/v1/jp/peer", "fixture-peer-only".into());
    let (url, server) = peer_http_server(app).await;
    let transport = Client::new(
        &url,
        "fixture-peer-only",
        Region::Jp,
        true,
        true,
        Policy::default(),
    )
    .unwrap();
    let response = transport
        .call(
            &request,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(response.request_id, request.request_id);
    assert!(
        matches!(response.outcome, crate::peer::Outcome::Success { data } if data["version"] == "master-fixture")
    );
    let wrong = Client::new(
        &url,
        "wrong-peer",
        Region::Jp,
        true,
        true,
        Policy::default(),
    )
    .unwrap();
    assert!(matches!(
        wrong
            .call(
                &request,
                tokio::time::Instant::now() + Duration::from_secs(2)
            )
            .await,
        Err(Error::Status(401))
    ));
    assert_eq!(f.received.lock().unwrap().len(), 1);
    assert!(f.received.lock().unwrap()[0]
        .1
        .get("authorization")
        .is_none());
    let mut other = outgoing_peer_request();
    other.identity.region = Region::Tw;
    assert!(matches!(
        transport
            .call(&other, tokio::time::Instant::now() + Duration::from_secs(2))
            .await,
        Err(Error::Config)
    ));
    server.abort();
}
#[tokio::test]
async fn peer_transport_rejects_unbound_malformed_and_oversized_replies_without_retry() {
    use crate::{
        peer_transport::{Client, Error, Policy},
        region::Region,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    for case in 0..11 {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let app = axum::Router::new().route("/internal/v1/peer/query", axum::routing::post(move |headers: HeaderMap, axum::Json(request): axum::Json<Value>| {
            let counter = counter.clone(); async move {
                counter.fetch_add(1, Ordering::Relaxed);
                assert_eq!(headers["authorization"], "Bearer fixture-peer-only");
                assert_eq!(headers["content-type"], "application/json");
                let mut reply = json!({"request_id":request["request_id"],"identity":request["identity"],"observation":crate::client::Observation::default(),"outcome":{"status":"success","data":{"largeId":"9223372036854775807"}}});
                let mut mime = "application/json";
                match case {
                    0 => {},
                    1 => reply["request_id"] = json!(uuid::Uuid::new_v4().to_string()),
                    2 => reply["identity"]["region"] = json!("en"),
                    3 => reply["identity"]["protocol_sha256"] = json!("0".repeat(64)),
                    4 => reply["outcome"] = json!({"status":"failure","kind":{"type":"timeout","extra":"SECRET"}}),
                    5 => reply["outcome"] = json!({"status":"failure","kind":{"type":"game","grpc_status":0}}),
                    6 => reply["outcome"]["data"] = Value::Null,
                    7 => reply["unknown"] = json!("SECRET"),
                    8 => mime = "text/html",
                    9 | 10 => reply["outcome"]["data"]["padding"] = json!("X".repeat(2048)),
                    _ => unreachable!(),
                }
                let bytes = serde_json::to_vec(&reply).unwrap();
                let body = if case == 10 {
                    axum::body::Body::from_stream(stream::iter(bytes.chunks(512).map(|b| Ok::<_, Infallible>(Bytes::copy_from_slice(b))).collect::<Vec<_>>()))
                } else { axum::body::Body::from(bytes) };
                axum::response::Response::builder().header("content-type", mime).body(body).unwrap()
            }
        }));
        let (url, server) = peer_http_server(app).await;
        let policy = Policy {
            max_response_bytes: 1024,
            ..Default::default()
        };
        let transport =
            Client::new(&url, "fixture-peer-only", Region::Jp, false, true, policy).unwrap();
        let result = transport
            .call(
                &outgoing_peer_request(),
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await;
        if case == 0 {
            assert!(
                matches!(result.unwrap().outcome, crate::peer::Outcome::Success { data } if data["largeId"] == "9223372036854775807")
            );
        } else {
            assert!(matches!(result, Err(Error::Protocol)), "case {case}");
        }
        assert_eq!(seen.load(Ordering::Relaxed), 1);
        server.abort();
    }
}
#[tokio::test]
async fn peer_transport_redirect_deadline_and_connection_failures_are_distinct() {
    use crate::{
        peer_transport::{Client, Error, Policy},
        region::Region,
    };
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect = format!("http://{}/leak", destination.local_addr().unwrap());
    let (url, server) = peer_http_server(axum::Router::new().route(
        "/internal/v1/peer/query",
        axum::routing::post(move || {
            let redirect = redirect.clone();
            async move {
                axum::response::Response::builder()
                    .status(307)
                    .header("location", redirect)
                    .body(axum::body::Body::empty())
                    .unwrap()
            }
        }),
    ))
    .await;
    let transport = Client::new(
        &url,
        "fixture-peer-only",
        Region::Jp,
        false,
        true,
        Policy::default(),
    )
    .unwrap();
    let request = outgoing_peer_request();
    assert!(matches!(
        transport
            .call(
                &request,
                tokio::time::Instant::now() + Duration::from_secs(2)
            )
            .await,
        Err(Error::Status(307))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), destination.accept())
            .await
            .is_err()
    );
    server.abort();
    for (stalled_body, request_ms, deadline_ms) in [
        (false, 100, 1000),
        (true, 100, 1000),
        (false, 2000, 30),
        (true, 2000, 30),
    ] {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let app = axum::Router::new().route(
            "/internal/v1/peer/query",
            axum::routing::post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    if !stalled_body {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    let chunks =
                        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"{")) })
                            .chain(stream::once(async {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                Ok::<_, Infallible>(Bytes::from_static(b"}"))
                            }));
                    axum::response::Response::builder()
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from_stream(chunks))
                        .unwrap()
                }
            }),
        );
        let (url, server) = peer_http_server(app).await;
        let policy = Policy {
            connect_timeout_ms: 100,
            request_timeout_ms: request_ms,
            ..Default::default()
        };
        let transport =
            Client::new(&url, "fixture-peer-only", Region::Jp, false, true, policy).unwrap();
        let start = tokio::time::Instant::now();
        let error = transport
            .call(&request, start + Duration::from_millis(deadline_ms))
            .await
            .err()
            .unwrap();
        assert!(matches!(error, Error::Timeout));
        assert!(!error.definitely_not_sent());
        assert!(start.elapsed() < Duration::from_secs(1));
        let error = transport
            .call(
                &request,
                tokio::time::Instant::now() - Duration::from_millis(1),
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(error, Error::NotSent));
        assert!(error.definitely_not_sent());
        assert_eq!(seen.load(Ordering::Relaxed), 1);
        server.abort();
    }
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let transport = Client::new(
        &url,
        "fixture-peer-only",
        Region::Jp,
        false,
        true,
        Policy::default(),
    )
    .unwrap();
    let error = transport
        .call(
            &request,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, Error::Connect));
    assert!(error.definitely_not_sent());
}
#[test]
fn peer_transport_configuration_rejects_unscoped_credentials_and_bounds() {
    use crate::{
        peer_transport::{Client, Policy},
        region::Region,
    };
    for origin in [
        "http://example.invalid",
        "https://user:SECRET@example.invalid",
        "https://example.invalid/path",
        "https://example.invalid?q=SECRET",
        "https://example.invalid/#fragment",
        "https://example.invalid\\other",
        " https://example.invalid",
    ] {
        assert!(Client::new(origin, "token", Region::Jp, false, false, Policy::default()).is_err());
    }
    for token in ["", "with space", "bad\r\nvalue"] {
        assert!(Client::new(
            "https://example.invalid",
            token,
            Region::Jp,
            false,
            false,
            Policy::default()
        )
        .is_err());
    }
    assert!(Client::new(
        "https://example.invalid",
        "token",
        Region::Cn,
        false,
        false,
        Policy::default()
    )
    .is_err());
    assert!(Policy {
        max_response_bytes: usize::MAX,
        ..Default::default()
    }
    .validate()
    .is_err());
    assert!(Policy {
        connect_timeout_ms: 20_001,
        ..Default::default()
    }
    .validate()
    .is_err());
}

#[tokio::test]
async fn peer_transport_tls_rejection_and_mid_body_disconnect_preserve_delivery_uncertainty() {
    use crate::{
        peer_transport::{Client, Error, Policy},
        region::Region,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (url, rejected) = untrusted_tls_fixture().await;
    let transport = Client::new(
        &url,
        "fixture-peer-only",
        Region::Jp,
        false,
        false,
        Policy::default(),
    )
    .unwrap();
    let request = outgoing_peer_request();
    let error = transport
        .call(
            &request,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, Error::Connect));
    assert!(tokio::time::timeout(Duration::from_secs(2), rejected)
        .await
        .unwrap()
        .unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{").await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let transport = Client::new(
        &url,
        "fixture-peer-only",
        Region::Jp,
        false,
        true,
        Policy::default(),
    )
    .unwrap();
    let error = transport
        .call(
            &request,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, Error::Transport));
    assert!(!error.definitely_not_sent());
    assert!(!error.to_string().contains("fixture-peer-only"));
    server.await.unwrap();
}

fn routing_target(name: &str, origin: String, priority: i32) -> crate::node_routing::TargetConfig {
    let token_env = format!("SIRIUS_ROUTING_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&token_env, "node-secret");
    crate::node_routing::TargetConfig {
        name: name.into(),
        origin,
        priority,
        token_env,
        regional_paths: false,
        allow_http: true,
    }
}
#[tokio::test]
async fn node_routing_public_system_uses_remote_observation_without_contaminating_local_state() {
    let upstream = fixture(vec![Reply::version()]).await;
    let remote = client(&upstream, config());
    let (url, server) = peer_http_server(crate::peer::router(
        remote,
        "/internal/v1/peer",
        "node-secret".into(),
    ))
    .await;
    let local = fixture(vec![]).await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        local_priority: None,
        targets: vec![routing_target("remote", url, 10)],
        ..Default::default()
    });
    let front = client(&local, cfg);
    let app = api::router(front.clone(), "api".into(), "internal".into());
    let result = app
        .oneshot(
            Request::get("/api/v1/system")
                .header("authorization", "Bearer api")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    let value = body(result).await;
    assert_eq!(value["status"], "available");
    assert_eq!(value["observation"]["master_version"], "master-fixture");
    assert!(front.observation().await.master_version.is_none());
    assert!(front.snapshot().await.is_err());
    assert!(local.received.lock().unwrap().is_empty());
    assert_eq!(upstream.received.lock().unwrap().len(), 1);
    server.abort();
}
#[tokio::test]
async fn node_routing_local_account_rejection_fails_over_before_game_dispatch() {
    let upstream = fixture(vec![Reply::version(), empty_profile_reply()]).await;
    let remote = client(&upstream, account_config());
    let (url, server) = peer_http_server(crate::peer::router(
        remote,
        "/internal/v1/peer",
        "node-secret".into(),
    ))
    .await;
    let local = fixture(vec![]).await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        targets: vec![routing_target("remote", url, 10)],
        ..Default::default()
    });
    let front = client(&local, cfg);
    assert!(front
        .public_call(crate::peer::Operation::Profile { profile_id: 1 })
        .await
        .is_ok());
    assert!(local.received.lock().unwrap().is_empty());
    assert_eq!(upstream.received.lock().unwrap().len(), 2);
    server.abort();
}
async fn routing_mock(
    name: &'static str,
    mode: Arc<std::sync::atomic::AtomicUsize>,
    seen: Arc<std::sync::atomic::AtomicUsize>,
) -> (String, tokio::task::JoinHandle<()>) {
    use std::sync::atomic::Ordering;
    peer_http_server(axum::Router::new().route("/internal/v1/peer/query",axum::routing::post(move |axum::Json(request):axum::Json<Value>| {
        let mode=mode.clone();let seen=seen.clone();async move {
            seen.fetch_add(1,Ordering::Relaxed);let mode=mode.load(Ordering::Relaxed);
            if mode==4 {tokio::time::sleep(Duration::from_millis(150)).await;}
            let outcome=match mode {
                1=>json!({"status":"failure","kind":{"type":"transport"}}),
                2=>json!({"status":"failure","kind":{"type":"game","grpc_status":14}}),
                3=>json!({"status":"failure","kind":{"type":"identity_mismatch"}}),
                _=>json!({"status":"success","data":{"node":name,"myRank":99,"myScore":88}}),
            };
            axum::Json(json!({"request_id":request["request_id"],"identity":request["identity"],"observation":crate::client::Observation::default(),"outcome":outcome}))
        }
    }))).await
}
#[tokio::test]
async fn node_routing_priorities_cooldown_single_probe_and_terminal_game_errors() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mode = Arc::new(AtomicUsize::new(1));
    let first_seen = Arc::new(AtomicUsize::new(0));
    let second_seen = Arc::new(AtomicUsize::new(0));
    let (first, a) = routing_mock("first", mode.clone(), first_seen.clone()).await;
    let (second, b) =
        routing_mock("second", Arc::new(AtomicUsize::new(0)), second_seen.clone()).await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        local_priority: None,
        targets: vec![
            routing_target("second", second, 20),
            routing_target("first", first, 10),
        ],
        failure_threshold: 1,
        cooldown_ms: 100,
        ..Default::default()
    });
    let front = GameClient::new(cfg).unwrap();
    for _ in 0..2 {
        assert_eq!(
            front
                .public_call(crate::peer::Operation::Version {})
                .await
                .unwrap()["node"],
            "second"
        );
    }
    assert_eq!(first_seen.load(Ordering::Relaxed), 1);
    tokio::time::sleep(Duration::from_millis(120)).await;
    mode.store(4, Ordering::Relaxed);
    let copy = front.clone();
    let probe = tokio::spawn(async move {
        copy.public_call(crate::peer::Operation::Version {})
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while first_seen.load(Ordering::Relaxed) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for _ in 0..5 {
        assert_eq!(
            front
                .public_call(crate::peer::Operation::Version {})
                .await
                .unwrap()["node"],
            "second"
        );
    }
    assert_eq!(first_seen.load(Ordering::Relaxed), 2);
    assert_eq!(probe.await.unwrap()["node"], "first");
    let previous = second_seen.load(Ordering::Relaxed);
    mode.store(2, Ordering::Relaxed);
    assert!(matches!(
        front.public_call(crate::peer::Operation::Version {}).await,
        Err(AppError::Grpc(14))
    ));
    assert_eq!(second_seen.load(Ordering::Relaxed), previous);
    assert_eq!(front.node_status()["targets"][0]["failures"], 0);
    a.abort();
    b.abort();
}
#[tokio::test]
async fn node_routing_does_not_replay_ambiguous_authenticated_query_but_can_skip_incompatible_peer()
{
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mode = Arc::new(AtomicUsize::new(1));
    let first_seen = Arc::new(AtomicUsize::new(0));
    let second_seen = Arc::new(AtomicUsize::new(0));
    let (first, a) = routing_mock("first", mode.clone(), first_seen).await;
    let (second, b) =
        routing_mock("second", Arc::new(AtomicUsize::new(0)), second_seen.clone()).await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        local_priority: None,
        targets: vec![
            routing_target("first", first, 0),
            routing_target("second", second, 10),
        ],
        ..Default::default()
    });
    let front = GameClient::new(cfg).unwrap();
    assert!(matches!(
        front
            .public_call(crate::peer::Operation::Profile { profile_id: 1 })
            .await,
        Err(AppError::Transport)
    ));
    assert_eq!(second_seen.load(Ordering::Relaxed), 0);
    mode.store(3, Ordering::Relaxed);
    assert_eq!(
        front
            .public_call(crate::peer::Operation::Profile { profile_id: 1 })
            .await
            .unwrap()["node"],
        "second"
    );
    a.abort();
    b.abort();
}
#[tokio::test]
async fn node_routing_total_deadline_stops_before_next_target_and_recovers_admission() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mode = Arc::new(AtomicUsize::new(4));
    let seen = Arc::new(AtomicUsize::new(0));
    let (first, a) = routing_mock("first", mode.clone(), Arc::new(AtomicUsize::new(0))).await;
    let (second, b) = routing_mock("second", Arc::new(AtomicUsize::new(0)), seen.clone()).await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        local_priority: None,
        targets: vec![
            routing_target("first", first, 0),
            routing_target("second", second, 10),
        ],
        timeout_ms: 100,
        max_inflight: 1,
        ..Default::default()
    });
    let front = GameClient::new(cfg).unwrap();
    assert!(matches!(
        front.public_call(crate::peer::Operation::Version {}).await,
        Err(AppError::Timeout)
    ));
    assert_eq!(seen.load(Ordering::Relaxed), 0);
    mode.store(0, Ordering::Relaxed);
    assert_eq!(
        front
            .public_call(crate::peer::Operation::Version {})
            .await
            .unwrap()["node"],
        "first"
    );
    a.abort();
    b.abort();
}

#[tokio::test]
async fn node_routing_configuration_and_admin_scope_are_enforced_at_deployment() {
    use crate::{deployment::DeploymentConfig, node_routing, region::Region};
    let mut cfg = regional_config(Region::Jp);
    let target = routing_target("remote", "https://node.example.invalid".into(), 10);
    let token_env = target.token_env.clone();
    cfg.node_routing = Some(node_routing::Config {
        targets: vec![target],
        ..Default::default()
    });
    for bad in ["public-jp", "internal-jp", "fixture-cdn-secret"] {
        std::env::set_var(&token_env, bad);
        assert!(DeploymentConfig::Single(Box::new(cfg.clone()))
            .prepare()
            .is_err());
    }
    std::env::set_var(&token_env, "node-secret");
    let app = DeploymentConfig::Single(Box::new(cfg.clone()))
        .prepare()
        .unwrap()
        .router;
    for (token, status) in [("public-jp", 401), ("internal-jp", 200)] {
        let response = app
            .clone()
            .oneshot(
                Request::get("/internal/v1/nodes")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == 200 {
            let value = body(response).await;
            assert_eq!(value["targets"][0]["name"], "local");
            assert!(!value.to_string().contains("node-secret"));
            assert!(!value.to_string().contains("example.invalid"));
        }
    }
    let mut tw = regional_config(Region::Tw);
    tw.node_routing = cfg.node_routing.clone();
    let deployment = crate::deployment::MultiConfig {
        logging: None,
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: None,
        access_log: None,
        regions: BTreeMap::from([("jp".into(), cfg.clone()), ("tw".into(), tw)]),
    };
    assert!(DeploymentConfig::Multi(Box::new(deployment))
        .prepare()
        .is_err());
    let mut routing = cfg.node_routing.take().unwrap();
    routing.targets.push(routing.targets[0].clone());
    assert!(routing.validate(Region::Jp).is_err());
    routing.targets.clear();
    routing.local_priority = None;
    assert!(routing.validate(Region::Jp).is_err());
}

#[tokio::test]
async fn node_routing_local_priority_tie_and_incoming_peer_never_forward() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let seen = Arc::new(AtomicUsize::new(0));
    let (url, server) = routing_mock("remote", Arc::new(AtomicUsize::new(0)), seen.clone()).await;
    let local = fixture(vec![Reply::version(), Reply::version()]).await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        targets: vec![routing_target("remote", url, 0)],
        ..Default::default()
    });
    let front = client(&local, cfg.clone());
    assert_eq!(
        front
            .public_call(crate::peer::Operation::Version {})
            .await
            .unwrap()["version"],
        "master-fixture"
    );
    cfg.node_routing.as_mut().unwrap().targets[0].priority = -1;
    let front = client(&local, cfg);
    assert_eq!(
        front
            .public_call(crate::peer::Operation::Version {})
            .await
            .unwrap()["node"],
        "remote"
    );
    let app = crate::peer::router(front.clone(), "/internal/v1/peer", "peer".into());
    let request = peer_request(front.peer_identity().unwrap(), json!({"type":"version"}));
    assert_eq!(peer_send(app, "peer", request).await.status(), 200);
    assert_eq!(seen.load(Ordering::Relaxed), 1);
    assert_eq!(local.received.lock().unwrap().len(), 2);
    server.abort();
}

#[tokio::test]
async fn node_routing_cancelled_request_releases_bounded_admission() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mode = Arc::new(AtomicUsize::new(4));
    let seen = Arc::new(AtomicUsize::new(0));
    let (url, server) = routing_mock("remote", mode.clone(), seen.clone()).await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        local_priority: None,
        max_inflight: 1,
        targets: vec![routing_target("remote", url, 0)],
        ..Default::default()
    });
    let front = GameClient::new(cfg).unwrap();
    let copy = front.clone();
    let first =
        tokio::spawn(async move { copy.public_call(crate::peer::Operation::Version {}).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while seen.load(Ordering::Relaxed) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let copy = front.clone();
    let second =
        tokio::spawn(async move { copy.public_call(crate::peer::Operation::Version {}).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(seen.load(Ordering::Relaxed), 1);
    mode.store(0, Ordering::Relaxed);
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap()["node"],
        "remote"
    );
    assert_eq!(seen.load(Ordering::Relaxed), 2);
    server.abort();
}

#[tokio::test]
async fn node_routing_public_ranking_strips_remote_account_fields() {
    use std::sync::atomic::AtomicUsize;
    let (url, server) = routing_mock(
        "remote",
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let mut cfg = config();
    cfg.node_routing = Some(crate::node_routing::Config {
        local_priority: None,
        targets: vec![routing_target("remote", url, 0)],
        ..Default::default()
    });
    let app = api::router(
        GameClient::new(cfg).unwrap(),
        "api".into(),
        "internal".into(),
    );
    let response = app
        .oneshot(
            Request::get("/api/v1/songs/1/rankings")
                .header("authorization", "Bearer api")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let data = body(response).await;
    assert_eq!(data["node"], "remote");
    assert!(data.get("myRank").is_none());
    assert!(data.get("myScore").is_none());
    server.abort();
}

fn registry_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    crate::master::ImportReceipt,
) {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input");
    let output = root.path().join("output");
    std::fs::create_dir(&input).unwrap();
    let (manifest, decoder, data) = master_fixture();
    std::fs::write(
        input.join("MasterManifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(input.join("MasterFixture.bin"), data).unwrap();
    let receipt = crate::master::import_directory(&input, &output, &decoder).unwrap();
    (root, input, output, receipt)
}
fn registry_scope() -> crate::master_registry::Scope {
    crate::master_registry::Scope {
        region: crate::region::Region::Jp,
        environment: "release".into(),
        platform: crate::region::Platform::Ios,
    }
}
#[test]
fn master_registry_indexes_raw_json_and_pins_old_snapshots_across_current_changes() {
    use crate::{master, master_registry as registry};
    let (_root, input, output, first) = registry_fixture();
    let document = registry::manifest(&output, None, registry_scope()).unwrap();
    let published: registry::PublishedManifest = serde_json::from_slice(&document.bytes).unwrap();
    assert_eq!(published.snapshot, first.snapshot);
    // Independent Python hashlib/json(sort_keys=True, compact separators) vectors.
    assert_eq!(
        published.files[0].sha256,
        "7f991b00bf418634d0afd203936c3d9f49101a11be8818361d0c4e04c8b50be9"
    );
    assert_eq!(
        published.content_sha256,
        "bd6ee90cf8032681e2001c44c481dd3aa2165fb74318496d51c5fe3a39769a71"
    );
    let bytes = include_bytes!("../tests/fixtures/master-synthetic.json");
    assert_eq!(published.files[0].size, bytes.len() as u64);
    assert_eq!(published.files[0].sha256, registry::digest(bytes));
    let (_, decoder, _) = master_fixture();
    let second = master::import_directory(&input, &output, &decoder).unwrap();
    assert_ne!(first.snapshot, second.snapshot);
    let next = registry::manifest(&output, None, registry_scope()).unwrap();
    let next_manifest: registry::PublishedManifest = serde_json::from_slice(&next.bytes).unwrap();
    assert_eq!(published.content_sha256, next_manifest.content_sha256);
    assert_ne!(document.etag, next.etag);
    assert_eq!(
        registry::manifest(&output, Some(&first.snapshot), registry_scope())
            .unwrap()
            .bytes,
        document.bytes
    );
    assert_eq!(
        registry::table(
            &output,
            &first.snapshot,
            "MasterFixture",
            &published.files[0].sha256
        )
        .unwrap()
        .bytes,
        bytes
    );
    let mut scope = registry_scope();
    scope.environment = "review".into();
    let other: registry::PublishedManifest = serde_json::from_slice(
        &registry::manifest(&output, Some(&first.snapshot), scope)
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_ne!(published.content_sha256, other.content_sha256);
    let mut source = published.source_manifest.clone();
    source.files[0].name = "MasterManifest.bin".into();
    assert!(master::Manifest::parse(&serde_json::to_vec(&source).unwrap()).is_err());
}
#[test]
fn master_registry_corruption_and_legacy_snapshots_have_explicit_integrity_behavior() {
    use crate::{master, master_registry as registry};
    let (_root, _input, output, receipt) = registry_fixture();
    let path = output.join(&receipt.snapshot);
    let first: registry::PublishedManifest = serde_json::from_slice(
        &registry::manifest(&output, None, registry_scope())
            .unwrap()
            .bytes,
    )
    .unwrap();
    let hash = &first.files[0].sha256;
    std::fs::write(path.join("MasterFixture.json"), b"[]").unwrap();
    assert!(matches!(
        master::read_current(&output, Some("MasterFixture")),
        Err(master::MasterError::Integrity)
    ));
    assert!(registry::table(&output, &receipt.snapshot, "MasterFixture", hash).is_err());
    std::fs::write(
        path.join("MasterFixture.json"),
        include_bytes!("../tests/fixtures/master-synthetic.json"),
    )
    .unwrap();
    std::fs::write(path.join("tables.json"), b"{}").unwrap();
    assert!(registry::manifest(&output, None, registry_scope()).is_err());
    // Missing indexes are the legacy 1.1 format; malformed existing indexes never use that fallback.
    std::fs::remove_file(path.join("tables.json")).unwrap();
    let legacy: registry::PublishedManifest = serde_json::from_slice(
        &registry::manifest(&output, None, registry_scope())
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(first.content_sha256, legacy.content_sha256);
    assert!(!path.join("tables.json").exists());
    assert!(registry::table(&output, &receipt.snapshot, "MasterFixture", hash).is_ok());
    std::fs::write(path.join("MasterFixture.json"), b"invalid JSON").unwrap();
    assert!(registry::manifest(&output, None, registry_scope()).is_err());
}
#[cfg(unix)]
#[test]
fn master_registry_rejects_symbolic_links_and_unlisted_files() {
    use crate::master_registry as registry;
    let (root, _input, output, receipt) = registry_fixture();
    let snapshot = output.join(&receipt.snapshot);
    let manifest: registry::PublishedManifest = serde_json::from_slice(
        &registry::manifest(&output, None, registry_scope())
            .unwrap()
            .bytes,
    )
    .unwrap();
    let outside = root.path().join("outside.json");
    std::fs::write(&outside, b"[]").unwrap();
    std::fs::remove_file(snapshot.join("MasterFixture.json")).unwrap();
    std::os::unix::fs::symlink(&outside, snapshot.join("MasterFixture.json")).unwrap();
    assert!(registry::table(
        &output,
        &receipt.snapshot,
        "MasterFixture",
        &manifest.files[0].sha256
    )
    .is_err());
    assert!(registry::table(&output, &receipt.snapshot, "receipt", &"0".repeat(64)).is_err());
    assert!(registry::manifest(&output, Some("../outside"), registry_scope()).is_err());
    std::os::unix::fs::symlink(&snapshot, output.join("master-link")).unwrap();
    assert!(registry::manifest(&output, Some("master-link"), registry_scope()).is_err());
    std::fs::remove_file(snapshot.join("tables.json")).unwrap();
    std::os::unix::fs::symlink(&outside, snapshot.join("tables.json")).unwrap();
    assert!(registry::manifest(&output, None, registry_scope()).is_err());
}
#[tokio::test]
async fn master_registry_http_auth_conditional_reads_and_pinned_bytes_work_without_game_calls() {
    let (_root, _input, output, receipt) = registry_fixture();
    let f = fixture(vec![]).await;
    let mut cfg = config();
    cfg.master_directory = Some(output.clone());
    let app = api::router(client(&f, cfg), "api".into(), "internal".into());
    let get = |path: &str, token: &str, etag: Option<&str>| {
        let mut request = Request::get(path).header("authorization", format!("Bearer {token}"));
        if let Some(etag) = etag {
            request = request.header("if-none-match", etag);
        }
        request.body(axum::body::Body::empty()).unwrap()
    };
    let path = "/api/v1/master-data/manifest";
    assert_eq!(
        app.clone()
            .oneshot(get(path, "internal", None))
            .await
            .unwrap()
            .status(),
        401
    );
    let response = app.clone().oneshot(get(path, "api", None)).await.unwrap();
    assert_eq!(response.status(), 200);
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let manifest = body(response).await;
    assert_eq!(manifest["scope"]["region"], "jp");
    let response = app
        .clone()
        .oneshot(get(path, "api", Some(&etag)))
        .await
        .unwrap();
    assert_eq!(response.status(), 304);
    assert!(response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty());
    let blob = format!(
        "/api/v1/master-data/snapshots/{}/tables/MasterFixture/{}",
        receipt.snapshot,
        manifest["files"][0]["sha256"].as_str().unwrap()
    );
    let response = app.clone().oneshot(get(&blob, "api", None)).await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers()["cache-control"]
        .to_str()
        .unwrap()
        .contains("immutable"));
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        include_bytes!("../tests/fixtures/master-synthetic.json")
    );
    std::fs::write(
        output.join(&receipt.snapshot).join("MasterFixture.json"),
        b"[]",
    )
    .unwrap();
    assert_eq!(
        app.oneshot(get(&blob, "api", None)).await.unwrap().status(),
        503
    );
    assert!(f.received.lock().unwrap().is_empty());
}

fn master_sync_config(origin: String, output: std::path::PathBuf) -> Config {
    let mut cfg = config();
    cfg.master_directory = Some(output);
    let token_env = format!("SIRIUS_MASTER_SYNC_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&token_env, "owner-read");
    cfg.master_sync = Some(crate::master_sync::Config {
        origin,
        token_env,
        regional_paths: false,
        allow_http: true,
        interval_seconds: 60,
        timeout_seconds: 5,
        request_timeout_ms: 2000,
    });
    cfg
}
#[tokio::test]
async fn master_sync_installs_reuses_verifies_and_repairs_without_game_credentials() {
    let (_owner_root, input, output, _) = registry_fixture();
    let game = fixture(vec![]).await;
    let mut cfg = config();
    cfg.master_directory = Some(output.clone());
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = calls.clone();
    let app = api::router(
        client(&game, cfg),
        "owner-read".into(),
        "owner-admin".into(),
    )
    .layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let seen = seen.clone();
            async move {
                assert_eq!(request.headers()["authorization"], "Bearer owner-read");
                seen.lock().unwrap().push(request.uri().path().to_owned());
                next.run(request).await
            }
        },
    ));
    let (origin, server) = peer_http_server(app).await;
    let consumer = tempfile::tempdir().unwrap();
    let cfg = master_sync_config(origin, consumer.path().join("master"));
    let front = GameClient::new(cfg.clone()).unwrap();
    let sync = crate::master_sync::Syncer::new(&cfg, front.clone()).unwrap();
    let first = sync.update_once().await.unwrap();
    assert_eq!(first["action"], "updated");
    assert_eq!(first["downloaded_files"], 1);
    let installed = crate::master::read_current(
        cfg.master_directory.as_ref().unwrap(),
        Some("MasterFixture"),
    )
    .unwrap();
    assert_eq!(
        installed.bytes,
        include_bytes!("../tests/fixtures/master-synthetic.json")
    );
    calls.lock().unwrap().clear();
    assert_eq!(sync.update_once().await.unwrap()["action"], "unchanged");
    assert_eq!(calls.lock().unwrap().len(), 1);
    let root = cfg.master_directory.as_ref().unwrap();
    let current = crate::master_registry::current_snapshot(root).unwrap();
    std::fs::write(root.join(&current).join("MasterFixture.json"), b"[]").unwrap();
    let repair = sync.update_once().await.unwrap();
    assert_eq!(repair["downloaded_files"], 1);
    let (mut manifest, decoder, _) = master_fixture();
    manifest.version = "fixture-v2".into();
    std::fs::write(
        input.join("MasterManifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    crate::master::import_directory(&input, &output, &decoder).unwrap();
    calls.lock().unwrap().clear();
    let update = sync.update_once().await.unwrap();
    assert_eq!(update["receipt"]["version"], "fixture-v2");
    assert_eq!(update["downloaded_files"], 0);
    assert_eq!(update["reused_files"], 1);
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(root.join(current).exists());
    assert_eq!(front.master_update_status().await["mode"], "sync");
    assert!(game.received.lock().unwrap().is_empty());
    // A consumer can itself publish the same canonical content under its own snapshot UUID.
    let local: crate::master_registry::PublishedManifest = serde_json::from_slice(
        &crate::master_registry::manifest(root, None, registry_scope())
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(local.content_sha256, update["content_sha256"]);
    server.abort();
}
#[tokio::test]
async fn master_sync_rejects_wrong_scope_and_late_owner_change_preserving_installed_snapshot() {
    for wrong_scope in [true, false] {
        let (_owner_root, input, output, _) = registry_fixture();
        let (_consumer_root, _consumer_input, consumer, _) = registry_fixture();
        let pointer = std::fs::read(consumer.join("CURRENT")).unwrap();
        let (mut initial, decoder, _) = master_fixture();
        initial.version = "fixture-owner-v2".into();
        std::fs::write(
            input.join("MasterManifest.json"),
            serde_json::to_vec(&initial).unwrap(),
        )
        .unwrap();
        crate::master::import_directory(&input, &output, &decoder).unwrap();
        let game = fixture(vec![]).await;
        let mut owner = config();
        owner.master_directory = Some(output.clone());
        if wrong_scope {
            owner.environment = "review".into();
        }
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = seen.clone();
        let app = api::router(client(&game, owner), "owner-read".into(), "admin".into()).layer(
            axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let count = count.clone();
                    let input = input.clone();
                    let output = output.clone();
                    async move {
                        if request.uri().path().ends_with("/manifest")
                            && count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 1
                            && !wrong_scope
                        {
                            let (mut manifest, decoder, _) = master_fixture();
                            manifest.version = "fixture-owner-v3".into();
                            std::fs::write(
                                input.join("MasterManifest.json"),
                                serde_json::to_vec(&manifest).unwrap(),
                            )
                            .unwrap();
                            crate::master::import_directory(&input, &output, &decoder).unwrap();
                        }
                        next.run(request).await
                    }
                },
            ),
        );
        let (origin, server) = peer_http_server(app).await;
        let cfg = master_sync_config(origin, consumer.clone());
        let sync =
            crate::master_sync::Syncer::new(&cfg, GameClient::new(cfg.clone()).unwrap()).unwrap();
        let error = sync.update_once().await.unwrap_err();
        if wrong_scope {
            assert!(matches!(error, crate::master_sync::Error::Integrity));
            assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 1);
        } else {
            assert!(matches!(error, crate::master_sync::Error::Changed));
        }
        assert_eq!(std::fs::read(consumer.join("CURRENT")).unwrap(), pointer);
        assert!(!std::fs::read_dir(&consumer).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".master-sync-")));
        server.abort();
    }
}
#[tokio::test]
async fn master_sync_shutdown_and_corrupted_download_never_publish_and_writer_recovers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_owner_root, _input, output, _) = registry_fixture();
    let game = fixture(vec![]).await;
    let mut owner = config();
    owner.master_directory = Some(output);
    let mode = Arc::new(AtomicUsize::new(1));
    let seen = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (flag, count, blocked) = (mode.clone(), seen.clone(), gate.clone());
    let app = api::router(client(&game, owner), "owner-read".into(), "admin".into()).layer(
        axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let flag = flag.clone();
                let count = count.clone();
                let blocked = blocked.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    if flag.load(Ordering::Relaxed) == 1 {
                        let _permit = blocked.acquire().await.unwrap();
                    }
                    if flag.load(Ordering::Relaxed) == 2
                        && request.uri().path().contains("/tables/")
                    {
                        return axum::response::Response::builder()
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from("[]"))
                            .unwrap();
                    }
                    next.run(request).await
                }
            },
        ),
    );
    let (origin, server) = peer_http_server(app).await;
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("consumer");
    let cfg = master_sync_config(origin, output.clone());
    let sync =
        crate::master_sync::Syncer::new(&cfg, GameClient::new(cfg.clone()).unwrap()).unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(sync.clone().run(receiver));
    tokio::time::timeout(Duration::from_secs(2), async {
        while seen.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    shutdown.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(!output.join("CURRENT").exists());
    mode.store(2, Ordering::Relaxed);
    gate.add_permits(1);
    assert!(matches!(
        sync.update_once().await,
        Err(crate::master_sync::Error::Integrity)
    ));
    assert!(!output.join("CURRENT").exists());
    mode.store(0, Ordering::Relaxed);
    assert_eq!(sync.update_once().await.unwrap()["action"], "updated");
    server.abort();
}

#[tokio::test]
async fn master_sync_configuration_enforces_scope_policy_and_worker_assembly() {
    use crate::{deployment::DeploymentConfig, region::Region};
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = regional_config(Region::Jp);
    cfg.master_directory = Some(dir.path().join("consumer"));
    cfg.master_sync =
        master_sync_config("https://owner.example.invalid".into(), dir.path().into()).master_sync;
    let base = cfg.master_sync.clone().unwrap();
    for origin in [
        "http://owner.example.invalid",
        "https://user:pass@owner.example.invalid",
        "https://owner.example.invalid/path",
        "https://owner.example.invalid/?secret=1",
        "https://owner.example.invalid/#fragment",
    ] {
        let mut policy = base.clone();
        policy.allow_http = false;
        policy.origin = origin.into();
        assert!(policy.validate().is_err());
    }
    for (interval, total, request) in [
        (59, 600, 60000),
        (86401, 600, 60000),
        (60, 0, 60000),
        (60, 3601, 60000),
        (60, 600, 99),
        (60, 600, 300001),
    ] {
        let mut policy = base.clone();
        policy.interval_seconds = interval;
        policy.timeout_seconds = total;
        policy.request_timeout_ms = request;
        assert!(policy.validate().is_err());
    }
    let mut missing = cfg.clone();
    missing.master_directory = None;
    assert!(missing.validate().is_err());
    for region in [Region::Tw, Region::En, Region::Kr, Region::Cn] {
        let mut wrong = cfg.clone();
        wrong.region = region;
        assert!(wrong.validate().is_err());
    }
    for token in ["internal-jp", "fixture-cdn-secret"] {
        std::env::set_var(&base.token_env, token);
        assert!(DeploymentConfig::Single(Box::new(cfg.clone()))
            .prepare()
            .is_err());
    }
    std::env::set_var(&base.token_env, "public-jp");
    let prepared = DeploymentConfig::Single(Box::new(cfg)).prepare().unwrap();
    assert_eq!(prepared.syncers.len(), 1);
    assert!(prepared.updaters.is_empty());
    assert!(!dir.path().join("consumer/CURRENT").exists());
}

#[tokio::test]
async fn master_sync_whole_deadline_bounds_stalled_owner_and_preserves_current() {
    let (_root, _input, output, _) = registry_fixture();
    let before = std::fs::read(output.join("CURRENT")).unwrap();
    let (origin, server) = peer_http_server(axum::Router::new().fallback(|| async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        axum::Json(serde_json::json!({}))
    }))
    .await;
    let mut cfg = master_sync_config(origin, output.clone());
    cfg.master_sync.as_mut().unwrap().timeout_seconds = 1;
    cfg.master_sync.as_mut().unwrap().request_timeout_ms = 5000;
    let syncer =
        crate::master_sync::Syncer::new(&cfg, GameClient::new(cfg.clone()).unwrap()).unwrap();
    let start = std::time::Instant::now();
    assert!(matches!(
        syncer.update_once().await,
        Err(crate::master_sync::Error::Timeout)
    ));
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
    assert_eq!(std::fs::read(output.join("CURRENT")).unwrap(), before);
    assert!(crate::master::WriterLock::acquire(&output).is_ok());
    server.abort();
}

#[test]
fn master_publication_history_follows_committed_chain_and_retains_reimports() {
    use crate::{master, master_registry as registry};
    let (_root, input, output, first) = registry_fixture();
    let (_, decoder, _) = master_fixture();
    let second = master::import_directory(&input, &output, &decoder).unwrap();
    let orphan = master::import_directory(&input, &output, &decoder).unwrap();
    // Model a directory retained after a failed CURRENT switch: existence alone
    // must not turn it into a committed history entry.
    std::fs::write(output.join("CURRENT"), &second.snapshot).unwrap();
    let history = registry::history(&output, registry_scope(), 100).unwrap();
    assert_eq!(history.head, second.snapshot);
    assert_eq!(history.entries.len(), 2);
    assert_eq!(history.entries[0].snapshot, second.snapshot);
    assert_eq!(history.entries[1].snapshot, first.snapshot);
    assert!(history
        .entries
        .iter()
        .all(|entry| entry.snapshot != orphan.snapshot));
    assert_eq!(
        history.entries[0].content_sha256,
        history.entries[1].content_sha256
    );
    assert!(history
        .entries
        .iter()
        .all(|entry| entry.published_at.is_some() && entry.file_count == 1));
    assert!(!history.has_more);
    assert!(!history.legacy_boundary);
    let bounded = registry::history(&output, registry_scope(), 1).unwrap();
    assert_eq!(bounded.entries.len(), 1);
    assert!(bounded.has_more);
    assert!(registry::history(&output, registry_scope(), 0).is_err());
    assert!(registry::history(&output, registry_scope(), 101).is_err());
    let mut source: Value =
        serde_json::from_slice(&std::fs::read(input.join("MasterManifest.json")).unwrap()).unwrap();
    source["version"] = json!("history-v2");
    std::fs::write(
        input.join("MasterManifest.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
    let third = master::import_directory(&input, &output, &decoder).unwrap();
    let history = registry::history(&output, registry_scope(), 100).unwrap();
    assert_ne!(
        history.entries[0].content_sha256,
        history.entries[1].content_sha256
    );
    assert_ne!(
        history.entries[0].content_sha256,
        history.entries[1].content_sha256
    );
    assert_eq!(
        history
            .entries
            .iter()
            .map(|entry| entry.snapshot.as_str())
            .collect::<Vec<_>>(),
        vec![
            third.snapshot.as_str(),
            second.snapshot.as_str(),
            first.snapshot.as_str()
        ]
    );
    // Historical time is unknown for legacy snapshots; do not infer it from mtimes.
    std::fs::remove_file(output.join(&second.snapshot).join("publication.json")).unwrap();
    let legacy = registry::history(&output, registry_scope(), 100).unwrap();
    assert_eq!(legacy.entries.len(), 2);
    assert!(legacy.legacy_boundary);
    assert!(!legacy.has_more);
    assert!(legacy.entries[1].published_at.is_none());
}

#[test]
fn master_publication_history_rejects_corruption_cycles_and_unsafe_predecessors() {
    use crate::{master, master_registry as registry};
    let (_root, input, output, first) = registry_fixture();
    let (_, decoder, _) = master_fixture();
    let publication_path = output.join(&first.snapshot).join("publication.json");
    let original = std::fs::read(&publication_path).unwrap();
    for previous in [first.snapshot.as_str(), "../escape", "master-missing"] {
        let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
        record["previous_snapshot"] = json!(previous);
        std::fs::write(&publication_path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(registry::history(&output, registry_scope(), 100).is_err());
    }
    std::fs::write(&publication_path, b"{}").unwrap();
    assert!(registry::history(&output, registry_scope(), 100).is_err());
    std::fs::write(&publication_path, &original).unwrap();
    #[cfg(unix)]
    {
        std::fs::remove_file(&publication_path).unwrap();
        std::os::unix::fs::symlink(
            output.join(&first.snapshot).join("receipt.json"),
            &publication_path,
        )
        .unwrap();
        assert!(registry::history(&output, registry_scope(), 100).is_err());
        std::fs::remove_file(&publication_path).unwrap();
        std::fs::write(&publication_path, &original).unwrap();
    }
    // Invalid existing CURRENT cannot silently create a new history root.
    std::fs::write(output.join("CURRENT"), b"../escape").unwrap();
    assert!(master::import_directory(&input, &output, &decoder).is_err());
    assert_eq!(std::fs::read(output.join("CURRENT")).unwrap(), b"../escape");
    assert_eq!(
        std::fs::read_dir(&output)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("master-"))
            .count(),
        1
    );
}

#[tokio::test]
async fn master_publication_history_http_is_authorized_bounded_and_local() {
    let (_root, _input, output, first) = registry_fixture();
    let game = fixture(vec![]).await;
    let mut cfg = config();
    cfg.master_directory = Some(output);
    let app = api::router(client(&game, cfg), "api".into(), "internal".into());
    for (query, token, expected) in [
        ("", "wrong", 401),
        ("?limit=0", "api", 400),
        ("?limit=101", "api", 400),
        ("?unknown=1", "api", 400),
        ("?limit=1", "api", 200),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/master-data/history{query}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        if expected == 200 {
            assert_eq!(response.headers()["cache-control"], "private, no-store");
            let bytes = axum::body::to_bytes(response.into_body(), 16384)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["head"], first.snapshot);
            assert_eq!(value["entries"][0]["snapshot"], first.snapshot);
            assert_eq!(value["scope"]["region"], "jp");
        }
    }
    assert!(game.received.lock().unwrap().is_empty());
}
