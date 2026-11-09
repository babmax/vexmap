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

#[test]
fn uuids_are_written_the_way_the_api_reads_them() {
    let id: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;
    assert_eq!(vexmap_link::uuid_text(id), "01234567-89ab-cdef-0123-456789abcdef");
    assert_eq!(vexmap_link::uuid_text(0), "00000000-0000-0000-0000-000000000000");
    assert_eq!(vexmap_link::uuid_text(id).parse::<uuid::Uuid>().unwrap().as_u128(), id);
}

#[test]
fn a_reply_is_read_for_status_and_id() {
    use vexmap_link::parse_edit_reply;
    let ok = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"outcome\":\"applied\",\"seq\":42}";
    assert_eq!(parse_edit_reply(ok), Ok((200, 42, false)));
    let replay = b"HTTP/1.1 200 OK\r\n\r\n{\"outcome\":\"replayed\",\"seq\":7}";
    assert_eq!(parse_edit_reply(replay), Ok((200, 7, true)));
    let no = b"HTTP/1.1 403 Forbidden\r\n\r\n{\"error\":\"forbidden\",\"message\":\"x\"}";
    assert_eq!(parse_edit_reply(no), Ok((403, 0, false)));
    assert!(parse_edit_reply(b"garbage").is_err());
    assert!(parse_edit_reply(b"HTTP/1.1 abc\r\n\r\n").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_reach_the_sidecar_as_the_player_and_the_database_decides() {
    use vexmap_core::{Category, Role};
    use vexmap_link::{EditJob, EditQueue, LinkEdit, start_edits};

    let (core, addr, _dir) = serve().await;
    let system = |key: &str| CommandContext {
        actor: Actor::System(ClientName::new("test").unwrap()),
        region: RegionId::global(),
        request_id: RequestId::new(key).unwrap(),
    };
    let admin = uuid::Uuid::new_v4();
    let nobody = uuid::Uuid::new_v4();
    core.set_role(&system("admin"), admin, Role::Admin, Default::default()).await.unwrap();
    core.set_role(&system("mod"), nobody, Role::Moderator, [Category::Wiki].into()).await.unwrap();

    let queue: &'static EditQueue = Box::leak(Box::new(EditQueue::new()));
    start_edits(config(addr, "a"), queue).unwrap();
    let job = |player: uuid::Uuid, token: u32, edit| EditJob { player: player.as_u128(), token, edit };
    assert!(queue.push(job(admin, 1, LinkEdit::AddNode { x: 0, z: 0, transfer: false })));
    assert!(queue.push(job(admin, 2, LinkEdit::AddNode { x: 100, z: 0, transfer: true })));
    assert!(queue.push(job(nobody, 3, LinkEdit::AddNode { x: 5, z: 5, transfer: false })));

    let mut done = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while done.len() < 3 && Instant::now() < deadline {
        queue.take_done(&mut done);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(done.len(), 3, "{done:?}");
    done.sort_by_key(|d| d.token);
    assert_eq!((done[0].http, done[1].http, done[2].http), (200, 200, 403));
    assert!(done[0].id > 0 && done[1].id > done[0].id && done[2].id == 0);

    // The line between the two new nodes, then the network holds what the admin drew and nothing of the moderator's.
    let line = LinkEdit::AddLine { a: done[0].id, b: done[1].id, mode: "walk", one_way: false };
    assert!(queue.push(job(admin, 4, line)));
    let mut more = Vec::new();
    while more.is_empty() && Instant::now() < deadline {
        queue.take_done(&mut more);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!((more[0].http, more[0].token), (200, 4));
    let network = core.route_network().await.unwrap();
    assert_eq!((network.nodes.len(), network.lines.len()), (2, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_sidecar_is_reported_not_hung() {
    use vexmap_link::{EditJob, EditQueue, LinkEdit, start_edits};
    // A port nothing listens on: bind, note the port, drop.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let queue: &'static EditQueue = Box::leak(Box::new(EditQueue::new()));
    start_edits(config(port, "a"), queue).unwrap();
    assert!(queue.push(EditJob { player: 1, token: 9, edit: LinkEdit::SetClosed { line: 1, closed: true } }));
    let mut done = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while done.is_empty() && Instant::now() < deadline {
        queue.take_done(&mut done);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!((done[0].http, done[0].token), (0, 9));
}

#[test]
fn the_queue_is_bounded() {
    use vexmap_link::{EditJob, EditQueue, LinkEdit, MAX_QUEUED_EDITS};
    let queue = EditQueue::new();
    let job = EditJob { player: 1, token: 1, edit: LinkEdit::SetClosed { line: 1, closed: true } };
    for _ in 0..MAX_QUEUED_EDITS {
        assert!(queue.push(job));
    }
    assert!(!queue.push(job));
}
