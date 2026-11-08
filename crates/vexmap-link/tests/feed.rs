//! The player feed: its wire format, the tick's mailbox, and the real sidecar on the other end of a real socket.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;
use vexmap_core::{Actor, ClientName, CommandContext, Core, CountryId, RegionId, RequestId, Role};
use vexmap_link::{
    Delivery, DeliveryBox, FeedBoxes, FeedConfig, FeedError, FeedPosition, MAX_BATCH_PLAYERS, Mailbox, PlayerSample,
    decode_batch, decode_deliveries, decode_hello, encode_batch, encode_deliveries, encode_hello, start_feed,
};
use vexmap_proto::{Message, parse_frame};
use vexmap_sidecar::PositionStore;
use vexmap_sidecar::fanout::run as run_fanout;
use vexmap_sidecar::feed::serve;

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
    tokio::spawn(serve(listener, core.clone(), store.clone()));
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

fn boxes(players: &'static Mailbox) -> FeedBoxes {
    FeedBoxes { players, deliveries: Box::leak(Box::new(DeliveryBox::new())) }
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
    let boxes = FeedBoxes { players: leaked_mailbox(), deliveries: Box::leak(Box::new(DeliveryBox::new())) };
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
    tokio::spawn(serve(listener, core, store.clone()));

    eventually("the newest batch arrives", async || player_count(&store) == 1).await;
}
