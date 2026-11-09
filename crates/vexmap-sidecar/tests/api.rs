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
    let app = router(core, OPEN, Default::default());
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
    let app = router(core, OPEN, Default::default());
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
    let app = router(core, OPEN, Default::default());

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
    let app = router(core, OPEN, Default::default());
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
    let app = router(core, OPEN, Default::default());
    let anonymous = Request::builder().uri("/v1/journal").header("host", "localhost").body(Body::empty()).unwrap();
    assert_eq!(send(&app, anonymous).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_foreign_host_is_refused_even_for_health() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN, Default::default());
    let rebound = Request::builder().uri("/healthz").header("host", "evil.example").body(Body::empty()).unwrap();
    assert_eq!(send(&app, rebound).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn health_needs_no_client_name_and_shows_the_head() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN, Default::default());
    let health = Request::builder().uri("/healthz").header("host", "localhost").body(Body::empty()).unwrap();
    assert_eq!(send(&app, health).await, (StatusCode::OK, json!({"status": "ok", "journal_head": 0})));
}

#[tokio::test]
async fn positions_start_empty_and_check_the_region() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN, Default::default());
    let (status, body) = send(&app, request("GET", "/v1/positions?region=eu-1", None, None)).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"regions": []})));
    let (status, _) = send(&app, request("GET", "/v1/positions?region=BAD%20NAME", None, None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_country_is_set_cleared_and_never_cleared_by_accident() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN, Default::default());
    let uri = format!("/v1/players/{}", Uuid::new_v4());
    let country_uri = format!("{uri}/country");

    let set = request("PUT", &country_uri, Some("c1"), Some(json!({"country": "north"})));
    assert_eq!(send(&app, set).await.0, StatusCode::OK);
    assert_eq!(send(&app, request("GET", &uri, None, None)).await.1["country"], "north");

    let forgotten = request("PUT", &country_uri, Some("c2"), Some(json!({})));
    assert_eq!(send(&app, forgotten).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let bad = request("PUT", &country_uri, Some("c3"), Some(json!({"country": "NOT VALID"})));
    assert_eq!(send(&app, bad).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(send(&app, request("GET", &uri, None, None)).await.1["country"], "north");

    let clear = request("PUT", &country_uri, Some("c4"), Some(json!({"country": null})));
    assert_eq!(send(&app, clear).await.0, StatusCode::OK);
    assert_eq!(send(&app, request("GET", &uri, None, None)).await.1["country"], Value::Null);
}

#[tokio::test]
async fn a_flooding_client_gets_429() {
    let (core, _dir) = core().await;
    let app = router(core, RateLimit { per_second: 1, burst: 2 }, Default::default());
    for _ in 0..2 {
        assert_eq!(send(&app, request("GET", "/v1/journal", None, None)).await.0, StatusCode::OK);
    }
    let (status, body) = send(&app, request("GET", "/v1/journal", None, None)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited")));
}

async fn draw(app: &Router, key: &str, uri: &str, body: Value) -> i64 {
    let (status, reply) = send(app, request("POST", uri, Some(key), Some(body))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["seq"].as_i64().unwrap()
}

#[tokio::test]
async fn a_network_is_drawn_and_a_route_is_built_over_it() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN, Default::default());
    let a = draw(&app, "n1", "/v1/network/nodes", json!({"x": 0, "z": 0})).await;
    let b = draw(&app, "n2", "/v1/network/nodes", json!({"x": 100, "z": 0})).await;
    let c = draw(&app, "n3", "/v1/network/nodes", json!({"x": 100, "z": -100})).await;
    draw(&app, "l1", "/v1/network/lines", json!({"a": a, "b": b, "mode": "walk"})).await;
    let second = draw(&app, "l2", "/v1/network/lines", json!({"a": b, "b": c, "mode": "walk"})).await;

    let (_, network) = send(&app, request("GET", "/v1/network", None, None)).await;
    assert_eq!((network["nodes"].as_array().unwrap().len(), network["lines"].as_array().unwrap().len()), (3, 2));

    let ask = json!({"stops": [{"x": 0, "z": 0}, {"x": 100, "z": -100}], "modes": ["walk"]});
    let (status, route) = send(&app, request("POST", "/v1/route", None, Some(ask.clone()))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(route["length"], 200.0);
    assert_eq!(route["hints"][0]["turn"], "left");
    assert_eq!(route["legs"][0]["mode"], "walk");

    // Closing the second road takes the network away from the end stop, so the walk there is a straight line.
    let closed = format!("/v1/network/lines/{second}/closed");
    let (status, _) = send(&app, request("PUT", &closed, Some("c1"), Some(json!({"closed": true})))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, detour) = send(&app, request("POST", "/v1/route", None, Some(ask.clone()))).await;
    assert!(detour["legs"].as_array().unwrap().iter().any(|leg| leg["kind"] == "direct"));
    // Without walking allowed there is no honest straight line, so the answer is "no route".
    let by_rail = json!({"stops": [{"x": 0, "z": 0}, {"x": 100, "z": -100}], "modes": ["train"]});
    let (status, body) = send(&app, request("POST", "/v1/route", None, Some(by_rail))).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("no_route")));
    // Reopening brings the cached graph up to date: the route is on the network again.
    send(&app, request("PUT", &closed, Some("c2"), Some(json!({"closed": false})))).await;
    let (_, again) = send(&app, request("POST", "/v1/route", None, Some(ask))).await;
    assert_eq!(again["length"], 200.0);
}

#[tokio::test]
async fn the_network_api_refuses_what_it_should() {
    let (core, _dir) = core().await;
    let app = router(core, OPEN, Default::default());
    let id = Uuid::new_v4();
    // A player with no powers cannot draw.
    let body = json!({"x": 1, "z": 1, "actor": id});
    let (status, _) = send(&app, request("POST", "/v1/network/nodes", Some("k1"), Some(body))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&app, request("POST", "/v1/network/nodes", None, Some(json!({"x": 1, "z": 1})))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a write needs an Idempotency-Key");
    let unknown = json!({"stops": [{"x": 0, "z": 0}, {"x": 1, "z": 1}], "modes": ["plane"]});
    assert_eq!(send(&app, request("POST", "/v1/route", None, Some(unknown))).await.0, StatusCode::BAD_REQUEST);
    let one = json!({"stops": [{"x": 0, "z": 0}]});
    assert_eq!(send(&app, request("POST", "/v1/route", None, Some(one))).await.0, StatusCode::BAD_REQUEST);
    let slow = json!({"stops": [{"x": 0, "z": 0}, {"x": 1, "z": 1}], "walk_speed": -1});
    assert_eq!(send(&app, request("POST", "/v1/route", None, Some(slow))).await.0, StatusCode::BAD_REQUEST);
    // Off the network a route is still a straight walk.
    let open = json!({"stops": [{"x": 0, "z": 0}, {"x": 30, "z": 40}]});
    let (status, route) = send(&app, request("POST", "/v1/route", None, Some(open))).await;
    assert_eq!((status, &route["legs"][0]["kind"], &route["length"]), (StatusCode::OK, &json!("direct"), &json!(50.0)));
}
