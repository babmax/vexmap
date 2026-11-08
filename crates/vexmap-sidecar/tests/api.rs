//! The router end to end on a real SQLite file, without opening a port.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;
use vexmap_core::Core;
use vexmap_sidecar::{RateLimit, bootstrap_owner, router};

const OPEN: RateLimit = RateLimit { per_second: 100_000, burst: 100_000 };

async fn core() -> (Core, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    (Core::connect(&url).await.unwrap(), dir)
}

fn request(method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7878")
        .header("x-vexmap-client", "test-mod");
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    match body {
        Some(body) => builder.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_player_is_recorded_promoted_and_journaled() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN);
    let id = Uuid::new_v4();

    let uri = format!("/v1/players/{id}");
    let (status, body) = send(&app, request("PUT", &uri, Some("k1"), Some(json!({"name": "Alex"})))).await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({"outcome": "applied", "seq": 1})));

    let role = json!({"role": "moderator", "categories": ["wiki", "places"]});
    let (status, _) = send(&app, request("PUT", &format!("{uri}/role"), Some("k2"), Some(role))).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(&app, request("GET", &uri, None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role"], "moderator");
    assert_eq!(body["categories"], json!(["places", "wiki"]));

    let (_, body) = send(&app, request("GET", "/v1/journal?after=1", None, None)).await;
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);
    assert_eq!(body["entries"][0]["kind"], "player.role_changed");
    assert_eq!(body["entries"][0]["actor"], "system:test-mod");
}

#[tokio::test]
async fn a_repeated_key_is_a_replay() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN);
    let uri = format!("/v1/players/{}", Uuid::new_v4());
    let body = json!({"name": "Alex"});
    send(&app, request("PUT", &uri, Some("same"), Some(body.clone()))).await;
    let (status, reply) = send(&app, request("PUT", &uri, Some("same"), Some(body))).await;
    assert_eq!((status, reply), (StatusCode::OK, json!({"outcome": "replayed", "seq": 1})));
}

#[tokio::test]
async fn the_actors_role_comes_from_the_database() {
    let (core, _dir) = core().await;
    let owner = Uuid::new_v4();
    bootstrap_owner(&core, owner).await.unwrap();
    bootstrap_owner(&core, owner).await.unwrap();
    assert_eq!(core.journal_head().await.unwrap(), 1, "bootstrap is idempotent");
    let app = router(core, OPEN);

    let nobody = Uuid::new_v4();
    let denied = json!({"enabled": true, "actor": nobody});
    let (status, body) = send(&app, request("PUT", "/v1/features/war", Some("f1"), Some(denied))).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")));

    let allowed = json!({"enabled": true, "actor": owner, "region": "a"});
    let (status, _) = send(&app, request("PUT", "/v1/features/war", Some("f2"), Some(allowed))).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = send(&app, request("GET", "/v1/features?region=a", None, None)).await;
    assert_eq!(body["modules"], json!(["war"]));
    assert_eq!(body["mask"], json!(1 << 9));
    let (_, body) = send(&app, request("GET", "/v1/features?region=b", None, None)).await;
    assert_eq!(body["mask"], json!(0));
}

#[tokio::test]
async fn bad_requests_are_refused_with_a_reason() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN);
    let id = Uuid::new_v4();
    let uri = format!("/v1/players/{id}");

    let no_key = request("PUT", &uri, None, Some(json!({"name": "Alex"})));
    assert_eq!(send(&app, no_key).await.0, StatusCode::BAD_REQUEST);

    let bad_name = request("PUT", &uri, Some("a"), Some(json!({"name": "no spaces"})));
    assert_eq!(send(&app, bad_name).await.0, StatusCode::BAD_REQUEST);

    let bad_id = request("GET", "/v1/players/not-a-uuid", None, None);
    assert_eq!(send(&app, bad_id).await.0, StatusCode::BAD_REQUEST);

    let unknown = request("PUT", "/v1/features/nope", Some("b"), Some(json!({"enabled": true})));
    assert_eq!(send(&app, unknown).await.0, StatusCode::NOT_FOUND);

    let absent = request("GET", &uri, None, None);
    assert_eq!(send(&app, absent).await.0, StatusCode::NOT_FOUND);

    let oversized = request("PUT", &uri, Some("c"), Some(json!({"name": "x".repeat(70 * 1024)})));
    assert_eq!(send(&app, oversized).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_client_must_name_itself() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN);
    let anonymous = Request::builder().uri("/v1/journal").header("host", "localhost").body(Body::empty()).unwrap();
    assert_eq!(send(&app, anonymous).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_foreign_host_is_refused_even_for_health() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN);
    let rebound = Request::builder().uri("/healthz").header("host", "evil.example").body(Body::empty()).unwrap();
    assert_eq!(send(&app, rebound).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn health_needs_no_client_name_and_shows_the_head() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN);
    let health = Request::builder().uri("/healthz").header("host", "localhost").body(Body::empty()).unwrap();
    assert_eq!(send(&app, health).await, (StatusCode::OK, json!({"status": "ok", "journal_head": 0})));
}

#[tokio::test]
async fn a_flooding_client_gets_429() {
    let (core, _dir) = core().await;
    let app = router(core, RateLimit { per_second: 1, burst: 2 });
    for _ in 0..2 {
        assert_eq!(send(&app, request("GET", "/v1/journal", None, None)).await.0, StatusCode::OK);
    }
    let (status, body) = send(&app, request("GET", "/v1/journal", None, None)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited")));
}
