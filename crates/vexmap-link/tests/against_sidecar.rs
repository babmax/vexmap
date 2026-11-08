//! The contract between the game's std-only client and the real sidecar, over a real socket.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use vexmap_core::{Actor, ClientName, CommandContext, Core, Module, RegionId, RequestId};
use vexmap_link::{Config, fetch_mask, parse_mask, start};
use vexmap_sidecar::{RateLimit, router};

async fn serve() -> (Core, SocketAddr, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    let core = Core::connect(&url).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(core.clone(), RateLimit { per_second: 1000, burst: 1000 }, Default::default());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (core, addr, dir)
}

async fn switch(core: &Core, module: Module, region: &RegionId, enabled: bool) {
    let ctx = CommandContext {
        actor: Actor::System(ClientName::new("test").unwrap()),
        region: RegionId::global(),
        request_id: RequestId::new(&uuid::Uuid::new_v4().to_string()).unwrap(),
    };
    core.set_feature(&ctx, module, region, enabled).await.unwrap();
}

fn config(addr: SocketAddr, region: &str) -> Config {
    Config::parse(Some(&addr.to_string()), Some(region)).unwrap()
}

/// Blocking calls leave the runtime's threads free for the server.
async fn fetch(config: Config) -> Result<u64, String> {
    tokio::task::spawn_blocking(move || fetch_mask(&config)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mask_follows_the_database_per_region() {
    let (core, addr, _dir) = serve().await;
    assert_eq!(fetch(config(addr, "a")).await, Ok(0));

    switch(&core, Module::Tiles, &RegionId::global(), true).await;
    switch(&core, Module::War, &RegionId::new("a").unwrap(), true).await;

    assert_eq!(fetch(config(addr, "a")).await, Ok(Module::Tiles.bit() | Module::War.bit()));
    assert_eq!(fetch(config(addr, "b")).await, Ok(Module::Tiles.bit()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_keeps_the_atomic_current() {
    let (core, addr, _dir) = serve().await;
    switch(&core, Module::Routing, &RegionId::global(), true).await;

    let features: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
    start(Config { every: Duration::from_millis(10), ..config(addr, "a") }, features).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while features.load(Ordering::Relaxed) != Module::Routing.bit() {
        assert!(Instant::now() < deadline, "the mask never arrived");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_sidecar_is_an_error_and_keeps_the_last_mask() {
    // Bind and drop: nothing listens on this port any more.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    assert!(fetch(config(addr, "a")).await.is_err());

    let features: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0b101)));
    start(Config { every: Duration::from_millis(5), ..config(addr, "a") }, features).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(features.load(Ordering::Relaxed), 0b101);
}

#[test]
fn only_a_200_with_a_number_is_a_mask() {
    assert_eq!(parse_mask(b"HTTP/1.1 200 OK\r\na: b\r\n\r\n{\"mask\":18,\"modules\":[]}"), Ok(18));
    assert_eq!(parse_mask(b"HTTP/1.1 200 OK\r\n\r\n{\"mask\":18}"), Ok(18));
    assert!(parse_mask(b"HTTP/1.1 429 Too Many\r\n\r\n{\"mask\":18}").is_err());
    assert!(parse_mask(b"HTTP/1.1 200 OK\r\n\r\n{\"error\":\"x\"}").is_err());
    assert!(parse_mask(b"HTTP/1.1 200 OK\r\n\r\n{\"mask\":}").is_err());
    assert!(parse_mask(b"HTTP/1.1 200 OK\r\n\r\n{\"mask\":99999999999999999999999}").is_err());
    assert!(parse_mask(b"HTTP/1.1 200 OK").is_err());
    assert!(parse_mask(&[0xff, 0xfe]).is_err());
}

#[test]
fn settings_cannot_leave_loopback_or_bend_the_request() {
    assert_eq!(Config::parse(None, None).unwrap().region, "global");
    assert!(Config::parse(Some("10.0.0.1:7878"), None).is_err());
    assert!(Config::parse(Some("nonsense"), None).is_err());
    for region in ["", "A", "a b", "a&x=1", "a\r\nHost: x", &"x".repeat(33)] {
        assert!(Config::parse(None, Some(region)).is_err(), "{region:?}");
    }
    assert!(Config::parse(Some("[::1]:7878"), Some("eu-1")).is_ok());
}
