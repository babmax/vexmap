//! The player feed: its wire format, the tick's mailbox, and the real sidecar on the other end of a real socket.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;
use vexmap_core::{Actor, ClientName, CommandContext, Core, CountryId, Outcome, RegionId, RequestId, Role, RouteMode};
use vexmap_link::{
    Delivery, DeliveryBox, FeedBoxes, FeedConfig, FeedError, FeedPosition, LIVE_CHUNK_BYTES, LiveChunk, LiveChunkBox,
    LivePlan, MAX_BATCH_PLAYERS, Mailbox, NetworkWant, NetworkWantBox, PlayerSample, TileDeliveryBox, TileRequest,
    TileRequestBox, decode_batch, decode_deliveries, decode_hello, decode_live_chunks, decode_network_wants,
    decode_tile_requests, encode_batch, encode_deliveries, encode_hello, encode_live_chunks, encode_network_wants,
    encode_tile_requests, is_live_chunks, is_network_wants, is_tile_requests, start_feed,
};
use vexmap_proto::{
    Column, LineMode, Material, Message, NetworkLine, NetworkNode, TILE_AREA, TileId, TileWant, decode_tile,
    parse_frame,
};
use vexmap_sidecar::PositionStore;
use vexmap_sidecar::fanout::run as run_fanout;
use vexmap_sidecar::feed::{Requests, serve};
use vexmap_sidecar::tiles::{TileConfig, start as start_tiles};
use vexmap_tiler::store::TileStore;

const OVERWORLD: &str = "minecraft:overworld";

fn sample(uuid: u128, name: &str) -> PlayerSample {
    PlayerSample {
        uuid,
        name: name.to_owned(),
        dimension: OVERWORLD.to_owned(),
        position: [10.5, 64.0, -3.25],
        yaw: 90.0,
    }
}

fn body(message: &[u8]) -> &[u8] {
    let length = u32::from_le_bytes([message[0], message[1], message[2], message[3]]) as usize;
    assert_eq!(length, message.len() - 4);
    &message[4..]
}

fn encoded(seq: u64, positions: bool, players: &[PlayerSample]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_batch(seq, positions, players, &mut out);
    out
}

#[test]
fn a_batch_survives_the_wire_at_an_eighth_of_a_block() {
    let mut nether = sample(2, "Steve");
    nether.dimension = "minecraft:the_nether".to_owned();
    nether.position = [-29_999_984.06, 319.99, 0.0624];
    nether.yaw = -90.0;
    let message = encoded(7, true, &[sample(1, "Alex"), nether, sample(3, "Bob")]);

    let batch = decode_batch(body(&message)).unwrap();
    assert_eq!((batch.seq, batch.positions, batch.players.len()), (7, true, 3));
    assert_eq!(batch.players[0].name, "Alex");
    let expected = FeedPosition { dimension: OVERWORLD.to_owned(), x: 84, y: 512, z: -26, yaw: 64 };
    assert_eq!(batch.players[0].position, Some(expected));
    let far = batch.players[1].position.as_ref().unwrap();
    assert_eq!(
        (far.dimension.as_str(), far.x, far.y, far.z, far.yaw),
        ("minecraft:the_nether", -239_999_872, 2560, 0, 192)
    );
    assert_eq!(batch.players[2].position.as_ref().unwrap().dimension, OVERWORLD);
}

#[test]
fn without_realtime_only_names_cross() {
    let batch = decode_batch(body(&encoded(1, false, &[sample(1, "Alex")]))).unwrap();
    assert!(!batch.positions);
    assert_eq!((batch.players[0].name.as_str(), &batch.players[0].position), ("Alex", &None));
}

#[test]
fn samples_that_cannot_be_represented_are_left_out() {
    let mut bad = Vec::new();
    for value in [f64::NAN, f64::INFINITY, 1e12] {
        let mut player = sample(1, "Bad");
        player.position[1] = value;
        bad.push(player);
    }
    bad.push(sample(2, "ThisNameIsFarTooLong"));
    bad.push(sample(3, ""));
    let mut long_dimension = sample(4, "Bad");
    long_dimension.dimension = "x".repeat(65);
    bad.push(long_dimension);
    bad.push(sample(5, "Good"));

    let batch = decode_batch(body(&encoded(1, true, &bad))).unwrap();
    let names: Vec<_> = batch.players.iter().map(|player| player.name.as_str()).collect();
    assert_eq!(names, ["Good"]);
}

#[test]
fn a_batch_is_capped() {
    let players: Vec<_> = (0..MAX_BATCH_PLAYERS as u128 + 10).map(|id| sample(id, "P")).collect();
    let batch = decode_batch(body(&encoded(1, true, &players))).unwrap();
    assert_eq!(batch.players.len(), MAX_BATCH_PLAYERS);
}

#[test]
fn every_truncation_is_an_error_and_never_a_panic() {
    let message = encoded(9, true, &[sample(1, "Alex"), sample(2, "Steve")]);
    let full = body(&message);
    for cut in 0..full.len() {
        assert!(decode_batch(&full[..cut]).is_err(), "cut at {cut}");
    }
    let mut longer = full.to_vec();
    longer.push(0);
    assert_eq!(decode_batch(&longer), Err(FeedError::TrailingBytes));
}

#[test]
fn hostile_counts_and_indexes_are_refused() {
    let mut too_many = vec![2u8];
    too_many.extend_from_slice(&[0; 8]);
    too_many.extend_from_slice(&[1, 0, 0xff, 0xff]);
    assert_eq!(decode_batch(&too_many), Err(FeedError::TooManyPlayers));

    let message = encoded(1, true, &[sample(1, "A")]);
    let mut wrong_index = body(&message).to_vec();
    let index = wrong_index.len() - 14;
    wrong_index[index] = 200;
    assert_eq!(decode_batch(&wrong_index), Err(FeedError::BadDimension));
}

#[test]
fn hello_carries_the_region_and_refuses_anything_else() {
    let mut out = Vec::new();
    encode_hello("eu-1", &mut out);
    assert_eq!(decode_hello(body(&out)), Ok("eu-1".to_owned()));
    assert_eq!(decode_hello(b"\x01XX\x01\x00"), Err(FeedError::BadHeader));
    assert_eq!(decode_hello(b"\x02VF\x01\x00"), Err(FeedError::BadHeader));
    assert_eq!(decode_hello(b""), Err(FeedError::Truncated));
}

#[test]
fn the_mailbox_keeps_only_the_newest_batch() {
    let mailbox = Mailbox::new();
    let mut read = Vec::new();
    assert_eq!(mailbox.take(&mut read), None);

    let mut first = vec![sample(1, "Old")];
    assert!(mailbox.publish(&mut first, true));
    assert!(first.is_empty(), "the empty slot came back");
    let mut second = vec![sample(2, "New")];
    assert!(mailbox.publish(&mut second, false));
    assert_eq!(second[0].name, "Old", "the replaced batch comes back for reuse");

    assert_eq!(mailbox.take(&mut read), Some((2, false)));
    assert_eq!(read[0].name, "New");
    assert_eq!(mailbox.take(&mut read), None, "a batch is read once");
    assert!(read.is_empty());
}

#[test]
fn an_empty_batch_is_news() {
    let mailbox = Mailbox::new();
    let mut read = Vec::new();
    assert!(mailbox.publish(&mut Vec::new(), true));
    assert_eq!(mailbox.take(&mut read), Some((1, true)));
}

#[test]
fn settings_stay_on_loopback() {
    assert_eq!(FeedConfig::parse(None, None).unwrap().addr.port(), 7879);
    assert!(FeedConfig::parse(Some("10.0.0.1:7879"), None).is_err());
    assert!(FeedConfig::parse(None, Some("a b")).is_err());
}

#[test]
fn deliveries_survive_the_wire_and_refuse_garbage() {
    let deliveries =
        vec![Delivery { viewers: vec![1, 2], frame: vec![9, 8, 7] }, Delivery { viewers: vec![], frame: vec![] }];
    let mut out = Vec::new();
    encode_deliveries(&deliveries, &mut out);
    let full = body(&out);
    assert_eq!(decode_deliveries(full), Ok(deliveries));
    for cut in 0..full.len() {
        assert!(decode_deliveries(&full[..cut]).is_err(), "cut at {cut}");
    }
    assert_eq!(decode_deliveries(&[3, 0xff, 0xff]), Err(FeedError::TooManyPlayers));
    assert_eq!(decode_deliveries(&[9]), Err(FeedError::BadHeader));
}

#[test]
fn the_delivery_box_keeps_the_newest_and_hands_it_over_once() {
    let deliveries = DeliveryBox::new();
    let mut read = Vec::new();
    assert!(!deliveries.try_take(&mut read));
    deliveries.put(vec![Delivery { viewers: vec![1], frame: vec![1] }]);
    deliveries.put(vec![Delivery { viewers: vec![2], frame: vec![2] }]);
    assert!(deliveries.try_take(&mut read));
    assert_eq!(read[0].viewers, [2]);
    read.clear();
    assert!(!deliveries.try_take(&mut read));
}

struct Sidecar {
    core: Core,
    store: Arc<PositionStore>,
    addr: SocketAddr,
    _dir: TempDir,
}

async fn sidecar() -> Sidecar {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    let core = Core::connect(&url).await.unwrap();
    let store = Arc::new(PositionStore::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, core.clone(), store.clone(), no_requests()));
    tokio::spawn(run_fanout(core.clone(), store.clone()));
    Sidecar { core, store, addr, _dir: dir }
}

async fn eventually(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done().await {
        assert!(Instant::now() < deadline, "never happened: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn feed_config(addr: SocketAddr, region: &str) -> FeedConfig {
    let parsed = FeedConfig::parse(Some(&addr.to_string()), Some(region)).unwrap();
    FeedConfig { retry: Duration::from_millis(20), poll: Duration::from_millis(5), ..parsed }
}

fn no_requests() -> Requests {
    Requests {
        tiles: tokio::sync::mpsc::channel(1).0,
        network: tokio::sync::mpsc::channel(1).0,
        live: tokio::sync::mpsc::channel(1).0,
    }
}

fn boxes(players: &'static Mailbox) -> FeedBoxes {
    FeedBoxes {
        network_wants: Box::leak(Box::new(NetworkWantBox::new())),
        live_chunks: Box::leak(Box::new(LiveChunkBox::new())),
        players,
        deliveries: Box::leak(Box::new(DeliveryBox::new())),
        tile_requests: Box::leak(Box::new(TileRequestBox::new())),
        tiles: Box::leak(Box::new(TileDeliveryBox::new())),
    }
}

fn leaked_mailbox() -> &'static Mailbox {
    Box::leak(Box::new(Mailbox::new()))
}

async fn connect(addr: SocketAddr, region: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut hello = Vec::new();
    encode_hello(region, &mut hello);
    stream.write_all(&hello).await.unwrap();
    stream
}

fn player_count(store: &PositionStore) -> usize {
    store.snapshot(None).iter().map(|region| region.players.len()).sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_carries_positions_and_records_players() {
    let sidecar = sidecar().await;
    let mailbox = leaked_mailbox();
    start_feed(feed_config(sidecar.addr, "eu-1"), boxes(mailbox)).unwrap();
    let uuid = Uuid::new_v4();

    let store = sidecar.store.clone();
    eventually("the position arrives", async || {
        mailbox.publish(&mut vec![sample(uuid.as_u128(), "Alex")], true);
        player_count(&store) == 1
    })
    .await;

    let regions = sidecar.store.snapshot(None);
    assert_eq!(regions[0].region, "eu-1");
    let player = &regions[0].players[0];
    assert_eq!((player.uuid, player.name.as_str(), player.dimension.as_str()), (uuid, "Alex", OVERWORLD));
    assert_eq!((player.x, player.y, player.z, player.yaw), (10.5, 64.0, -3.25, 90.0));

    let core = sidecar.core.clone();
    eventually("the player is recorded", async || {
        core.player(uuid).await.unwrap().is_some_and(|player| player.name.as_deref() == Some("Alex"))
    })
    .await;
    let head = sidecar.core.journal_head().await.unwrap();

    // The same player again, and again: no further writes.
    for _ in 0..5 {
        mailbox.publish(&mut vec![sample(uuid.as_u128(), "Alex")], true);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert_eq!(sidecar.core.journal_head().await.unwrap(), head);

    // Whoever is absent from the list has left.
    let store = sidecar.store.clone();
    eventually("the player leaves the table", async || {
        mailbox.publish(&mut Vec::new(), true);
        player_count(&store) == 0
    })
    .await;
    assert_eq!(sidecar.store.snapshot(None).len(), 1, "the region itself is still connected");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_realtime_off_players_are_recorded_but_not_located() {
    let sidecar = sidecar().await;
    let mut stream = connect(sidecar.addr, "a").await;
    let uuid = Uuid::new_v4();
    stream.write_all(&encoded(1, false, &[sample(uuid.as_u128(), "Alex")])).await.unwrap();

    let core = sidecar.core.clone();
    eventually("the player is recorded", async || core.player(uuid).await.unwrap().is_some()).await;
    assert_eq!(player_count(&sidecar.store), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rename_on_the_same_connection_is_recorded() {
    let sidecar = sidecar().await;
    let mut stream = connect(sidecar.addr, "a").await;
    let uuid = Uuid::new_v4();
    for name in ["Old", "New"] {
        stream.write_all(&encoded(1, true, &[sample(uuid.as_u128(), name)])).await.unwrap();
        let core = sidecar.core.clone();
        eventually("the name is stored", async || {
            core.player(uuid).await.unwrap().is_some_and(|player| player.name.as_deref() == Some(name))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_region_disappears_with_its_connection_but_not_with_an_older_one() {
    let sidecar = sidecar().await;
    let store = sidecar.store.clone();

    let mut old = connect(sidecar.addr, "a").await;
    old.write_all(&encoded(1, true, &[sample(1, "Old")])).await.unwrap();
    eventually("the first connection is live", async || player_count(&store) == 1).await;

    // The server restarted before the sidecar noticed the old socket die.
    let mut new = connect(sidecar.addr, "a").await;
    eventually("the new connection took over", async || player_count(&store) == 0).await;
    new.write_all(&encoded(1, true, &[sample(2, "New")])).await.unwrap();
    eventually("the new list is shown", async || player_count(&store) == 1).await;

    drop(old);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sidecar.store.snapshot(None)[0].players[0].name, "New");

    drop(new);
    eventually("the region is gone", async || store.snapshot(None).is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn garbage_ends_the_connection_and_nothing_else() {
    let sidecar = sidecar().await;
    let store = sidecar.store.clone();

    // No hello at all, and a declared length past the limit.
    let mut bad = TcpStream::connect(sidecar.addr).await.unwrap();
    bad.write_all(&u32::MAX.to_le_bytes()).await.unwrap();

    let mut stream = connect(sidecar.addr, "a").await;
    stream.write_all(&encoded(1, true, &[sample(1, "Alex")])).await.unwrap();
    eventually("the good server is unaffected", async || player_count(&store) == 1).await;

    stream.write_all(&[1, 0, 0, 0, 99]).await.unwrap();
    eventually("the bad batch ends its connection", async || store.snapshot(None).is_empty()).await;

    let mut invalid_region = TcpStream::connect(sidecar.addr).await.unwrap();
    let mut hello = Vec::new();
    encode_hello("NOT VALID", &mut hello);
    invalid_region.write_all(&hello).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sidecar.store.snapshot(None).is_empty());
}

fn system_context(request: &str) -> CommandContext {
    CommandContext {
        actor: Actor::System(ClientName::new("test").unwrap()),
        region: RegionId::global(),
        request_id: RequestId::new(request).unwrap(),
    }
}

/// The newest `PositionKey` that reaches `viewer`, as a list of player ids, while the tick keeps publishing `players`.
async fn next_key_for(
    viewer: u128,
    boxes: FeedBoxes,
    players: &[PlayerSample],
    wanted: impl Fn(&[u128]) -> bool,
) -> Vec<u128> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "no matching key frame reached the player");
        boxes.players.publish(&mut players.to_vec(), true);
        let mut read = Vec::new();
        if boxes.deliveries.try_take(&mut read) {
            for delivery in read.iter().filter(|delivery| delivery.viewers.contains(&viewer)) {
                let (header, data) = parse_frame(&delivery.frame).unwrap();
                let seen: Vec<u128> = match Message::decode(header.kind, data).unwrap() {
                    Message::PositionKey(frame) => frame.markers.iter().map(|marker| marker.uuid).collect(),
                    Message::PositionStreams(streams) if streams.is_empty() => Vec::new(),
                    _ => continue,
                };
                if wanted(&seen) {
                    return seen;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_country_reaches_its_members_and_no_one_else() {
    let sidecar = sidecar().await;
    let boxes = boxes(leaked_mailbox());
    start_feed(feed_config(sidecar.addr, "a"), boxes).unwrap();
    let (north_1, north_2, south, admin) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let north = CountryId::new("north").unwrap();
    let core = &sidecar.core;
    core.set_country(&system_context("1"), north_1, Some(north.clone())).await.unwrap();
    core.set_country(&system_context("2"), north_2, Some(north.clone())).await.unwrap();
    core.set_country(&system_context("3"), south, Some(CountryId::new("south").unwrap())).await.unwrap();
    core.set_role(&system_context("4"), admin, Role::Admin, Default::default()).await.unwrap();
    let players = [
        sample(north_1.as_u128(), "A"),
        sample(north_2.as_u128(), "B"),
        sample(south.as_u128(), "C"),
        sample(admin.as_u128(), "D"),
    ];

    let seen = next_key_for(north_1.as_u128(), boxes, &players, |seen| seen.len() >= 2).await;
    assert!(seen.contains(&north_1.as_u128()) && seen.contains(&north_2.as_u128()));
    assert_eq!(seen.len(), 2, "another country's member never reaches this frame");

    // A role or country changed in the database applies without any reconnect, and clears what they saw.
    core.set_country(&system_context("5"), north_1, None).await.unwrap();
    let seen = next_key_for(north_1.as_u128(), boxes, &players, |seen| seen.is_empty()).await;
    assert!(seen.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tile_request_crosses_the_real_socket_and_the_tile_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    let core = Core::connect(&url).await.unwrap();
    let store = Arc::new(PositionStore::default());
    let id = TileId { level: 0, x: 3, z: -1 };
    let mut pixels = Box::new([0_u8; TILE_AREA]);
    pixels.fill(0x82);
    TileStore::new(dir.path().join("tiles")).write(OVERWORLD, id, 11, &pixels).unwrap();
    let config = TileConfig { worlds: Vec::new(), store: dir.path().join("tiles") };
    let (requests, live) = start_tiles(config, store.clone(), Arc::default()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Requests { tiles: requests, network: tokio::sync::mpsc::channel(1).0, live };
    tokio::spawn(serve(listener, core.clone(), store.clone(), requests));
    let boxes = boxes(leaked_mailbox());
    start_feed(feed_config(addr, "a"), boxes).unwrap();
    eventually("the region connected", async || !store.snapshot(None).is_empty()).await;

    let viewer = 77_u128;
    let body = Message::TileWants { dimension: OVERWORLD.into(), wants: vec![TileWant { id, version: 0 }] }.body();
    assert!(boxes.tile_requests.try_push(TileRequest { viewer, body }));

    let mut received = Vec::new();
    eventually("the tile arrived", async || boxes.tiles.try_take(&mut received, 1 << 20)).await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].viewers, vec![viewer]);
    let (header, body) = parse_frame(&received[0].frame).unwrap();
    let Message::Tile { id: got, version, pixels: encoded, .. } = Message::decode(header.kind, body).unwrap() else {
        panic!("a tile");
    };
    assert_eq!((got, version), (id, 11));
    let mut decoded = Box::new([0_u8; TILE_AREA]);
    decode_tile(&encoded, &mut decoded).unwrap();
    assert_eq!(decoded, pixels);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_network_request_crosses_the_real_socket_and_a_page_comes_back_to_that_player() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    let core = Core::connect(&url).await.unwrap();
    let store = Arc::new(PositionStore::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Requests {
        tiles: tokio::sync::mpsc::channel(1).0,
        network: vexmap_sidecar::network::start_pages(core.clone(), store.clone(), Arc::default()),
        live: tokio::sync::mpsc::channel(1).0,
    };
    tokio::spawn(serve(listener, core.clone(), store.clone(), requests));
    let boxes = boxes(leaked_mailbox());
    start_feed(feed_config(addr, "a"), boxes).unwrap();
    eventually("the region connected", async || !store.snapshot(None).is_empty()).await;

    let admin = Uuid::new_v4();
    core.set_role(&system_context("role"), admin, Role::Admin, Default::default()).await.unwrap();
    let node = |outcome| match outcome {
        Outcome::Applied { seq } => seq,
        other => panic!("{other:?}"),
    };
    let first = node(core.add_route_node(&system_context("n1"), -5, 6, true).await.unwrap());
    let second = node(core.add_route_node(&system_context("n2"), 7, 8, false).await.unwrap());
    let line = node(core.add_route_line(&system_context("l1"), first, second, RouteMode::Train, true).await.unwrap());

    let ask = async |viewer: u128, since: u64| {
        assert!(boxes.network_wants.try_push(NetworkWant { viewer, since, render: false }));
        let mut received = Vec::new();
        eventually("the page arrived", async || boxes.tiles.try_take(&mut received, 1 << 20)).await;
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].viewers, vec![viewer], "the answer is for who asked");
        let (header, body) = parse_frame(&received[0].frame).unwrap();
        let Message::NetworkPage(page) = Message::decode(header.kind, body).unwrap() else { panic!("a page") };
        page
    };

    let page = ask(admin.as_u128(), 0).await;
    assert!(page.may_edit);
    assert_eq!((page.since, page.to, page.latest), (0, line as u64, line as u64));
    assert_eq!(page.nodes[0], NetworkNode { id: first as u64, x: -5, z: 6, transfer: true });
    assert_eq!(
        page.lines,
        vec![NetworkLine {
            id: line as u64,
            a: first as u64,
            b: second as u64,
            mode: LineMode::Train,
            one_way: true,
            closed: false,
        }]
    );

    // An ordinary player gets the same network and no editor. A current copy gets an empty page, so polling stays cheap.
    let page = ask(Uuid::new_v4().as_u128(), line as u64).await;
    assert!(!page.may_edit);
    assert_eq!(page.to, line as u64);
    assert!(page.nodes.is_empty() && page.lines.is_empty());

    // A `since` no database can hold is past the newest version, and does no harm.
    let page = ask(admin.as_u128(), u64::MAX).await;
    assert!(page.latest < page.since);
}

#[test]
fn the_tile_queues_are_bounded_and_hand_out_a_tick_at_a_time() {
    let tiles = TileDeliveryBox::new();
    let delivery = |size| Delivery { viewers: vec![1], frame: vec![0; size] };
    tiles.put((0..10).map(|_| delivery(1 << 20)).collect());
    let mut out = Vec::new();
    assert!(tiles.try_take(&mut out, 1 << 20));
    assert_eq!(out.len(), 1);
    out.clear();
    assert!(tiles.try_take(&mut out, usize::MAX));
    assert_eq!(out.len(), 7, "8 MiB is the bound: the two tiles past it were dropped");
    let requests = TileRequestBox::new();
    let request = TileRequest { viewer: 1, body: vec![0; 10] };
    assert!((0..512).all(|_| requests.try_push(request.clone())));
    assert!(!requests.try_push(request), "a full queue drops instead of growing");
    assert!(!requests.try_push(TileRequest { viewer: 1, body: vec![0; 2000] }), "nor does it take an oversized body");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chunk_from_the_running_game_crosses_the_socket_and_is_drawn() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    let core = Core::connect(&url).await.unwrap();
    let store = Arc::new(PositionStore::default());
    let config = TileConfig { worlds: Vec::new(), store: dir.path().join("tiles") };
    let (tiles, live) = start_tiles(config, store.clone(), Arc::default()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Requests { tiles, network: tokio::sync::mpsc::channel(1).0, live };
    tokio::spawn(serve(listener, core, store.clone(), requests));
    let boxes = boxes(leaked_mailbox());
    start_feed(feed_config(addr, "a"), boxes).unwrap();
    eventually("the region connected", async || !store.snapshot(None).is_empty()).await;

    let column = Column { material: Some(Material::Snow), height: 90, depth: 0 }.to_bytes();
    let columns = column.repeat(256);
    assert!(boxes.live_chunks.try_push(LiveChunk { dimension: OVERWORLD.into(), x: 2, z: 0, columns }));

    let stored = TileStore::new(dir.path().join("tiles"));
    let id = TileId { level: 0, x: 0, z: 0 };
    eventually("the chunk was drawn", async || stored.read(OVERWORLD, id).unwrap().is_some()).await;
}

#[test]
fn live_chunks_survive_the_wire_and_garbage_does_not() {
    let chunk = LiveChunk { dimension: OVERWORLD.into(), x: -3, z: 9, columns: vec![5; LIVE_CHUNK_BYTES] };
    let mut bytes = Vec::new();
    encode_live_chunks(std::slice::from_ref(&chunk), &mut bytes);
    assert!(is_live_chunks(&bytes[4..]));
    assert!(!is_network_wants(&bytes[4..]));
    assert_eq!(decode_live_chunks(&bytes[4..]).unwrap(), vec![chunk.clone()]);
    assert_eq!(decode_live_chunks(&bytes[4..bytes.len() - 1]), Err(FeedError::Truncated));
    let chunks = LiveChunkBox::new();
    assert!(!chunks.try_push(LiveChunk { columns: vec![0; 3], ..chunk.clone() }), "a short chunk never gets in");
    assert!((0..2048).all(|_| chunks.try_push(chunk.clone())));
    assert!(!chunks.try_push(chunk), "a full box drops instead of growing");
}

#[test]
fn a_round_goes_nearest_first_and_a_crowd_gets_smaller_circles() {
    let at = |x: f64| ("minecraft:overworld".to_owned(), x, 0.0);
    let mut plan = LivePlan::default();
    plan.refill(&[at(-1.0)]);
    assert_eq!(plan.pop(), Some(("minecraft:overworld".to_owned(), [-1, 0])), "the player's own chunk first");
    let mut count = 1;
    while plan.pop().is_some() {
        count += 1;
    }
    assert_eq!(count, 17 * 17, "radius 8");

    // Players close together share chunks instead of queueing them twice.
    plan.refill(&[at(0.0), at(16.0)]);
    let mut seen = std::collections::HashSet::new();
    while let Some(next) = plan.pop() {
        assert!(seen.insert(next), "each chunk once a round");
    }

    let crowd: Vec<_> = (0..40).map(|index| at(f64::from(index) * 10_000.0)).collect();
    plan.refill(&crowd);
    let mut count = 0;
    while plan.pop().is_some() {
        count += 1;
    }
    assert_eq!(count, 40 * 25, "the smallest circle");

    plan.refill(&[]);
    assert!(plan.is_empty());
}

#[test]
fn network_wants_survive_the_wire_and_garbage_does_not() {
    let wants = vec![
        NetworkWant { viewer: 7, since: 0, render: true },
        NetworkWant { viewer: u128::MAX, since: u64::MAX, render: false },
    ];
    let mut bytes = Vec::new();
    encode_network_wants(&wants, &mut bytes);
    assert!(is_network_wants(&bytes[4..]));
    assert!(!is_tile_requests(&bytes[4..]));
    assert_eq!(decode_network_wants(&bytes[4..]).unwrap(), wants);
    assert_eq!(decode_network_wants(&bytes[4..bytes.len() - 1]), Err(FeedError::Truncated));
}

#[test]
fn tile_requests_survive_the_wire_and_garbage_does_not() {
    let requests = vec![TileRequest { viewer: 5, body: vec![1, 2, 3] }, TileRequest { viewer: 6, body: Vec::new() }];
    let mut bytes = Vec::new();
    encode_tile_requests(&requests, &mut bytes);
    assert!(is_tile_requests(&bytes[4..]));
    assert_eq!(decode_tile_requests(&bytes[4..]).unwrap(), requests);
    assert_eq!(decode_tile_requests(&bytes[4..bytes.len() - 1]), Err(FeedError::Truncated));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thread_connects_once_the_sidecar_appears() {
    // Bind and drop: the port is free, and nothing listens yet.
    let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = free.local_addr().unwrap();
    drop(free);

    let mailbox = leaked_mailbox();
    start_feed(feed_config(addr, "a"), boxes(mailbox)).unwrap();
    mailbox.publish(&mut vec![sample(1, "Alex")], true);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    let core = Core::connect(&url).await.unwrap();
    let store = Arc::new(PositionStore::default());
    let listener = TcpListener::bind(addr).await.unwrap();
    tokio::spawn(serve(listener, core, store.clone(), no_requests()));

    eventually("the newest batch arrives", async || player_count(&store) == 1).await;
}
