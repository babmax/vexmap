// VexMap's glue between the protocol (`vexmap_proto`) and Horizon. Thin on purpose: logic lives in proto.rs, which is
// tested without the game. One bundle serves both sides: a dedicated server is `horizon-client --dedicated`, and each
// side only hears its own hooks.
//
// Phase 1 carries only the handshake and admission (ADR-007). Map features arrive behind feature bits in later phases.

use super::vexmap_editor as editor;
use super::vexmap_link as link;
use super::vexmap_proto as proto;
use super::vexmap_routing as routing;
use super::vexmap_screen as screen;
use super::vexmap_ui as ui;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub(super) const MOD_VERSION: &str = "0.1.0";
/// `Module::Realtime`. Bits never change (they are `Hello.features` on the wire).
const REALTIME: u64 = 1;
/// `Module::Routing`. A client that took it follows the route network and may send `NetworkEdit`.
const ROUTING: u64 = 1 << 2;
/// One player may send this many edits per second. The sidecar limits the whole server; this keeps one player from
/// using all of it.
const EDITS_PER_SECOND: u32 = 8;
/// `Module::Tiles`.
const TILES: u64 = 2;
/// Modules this client build implements, as bits of `Hello.features`. Each arrives with its phase.
pub(super) const FEATURES: u64 = REALTIME | TILES | ROUTING;

/// Modules switched on for this region, kept current by the sidecar link. All off until the sidecar first answers.
static SERVER_FEATURES: AtomicU64 = AtomicU64::new(0);
/// The newest list of this server's players, on its way to the sidecar.
static PLAYER_FEED: link::Mailbox = link::Mailbox::new();
/// Route network edits from players, on their way to the sidecar, and its answers on the way back.
static EDITS: link::EditQueue = link::EditQueue::new();

/// The newest frames for this server's players, on their way from the sidecar.
static DELIVERIES: link::DeliveryBox = link::DeliveryBox::new();
static TILE_REQUESTS: link::TileRequestBox = link::TileRequestBox::new();
static TILE_DELIVERIES: link::TileDeliveryBox = link::TileDeliveryBox::new();
static NETWORK_WANTS: link::NetworkWantBox = link::NetworkWantBox::new();
static LIVE_CHUNKS: link::LiveChunkBox = link::LiveChunkBox::new();
/// What the map shows for each block state, by state id: `Material` as a number, 0 where it looks through.
static BLOCK_MATERIALS: OnceLock<Vec<u8>> = OnceLock::new();
/// Chunks looked at per tick: 160 a second, so a player's whole circle every two seconds. Taking one is a pointer copy.
const LIVE_CHUNKS_PER_TICK: usize = 8;
/// Block states classified per tick while the table is built, so its cost is spread over a second or two.
const STATES_PER_TICK: u32 = 1024;
const LIVE_ROUND_EVERY_MS: u64 = 1000;
const LIVE_JOB_QUEUE: usize = 128;
const MAX_REMEMBERED_CHUNKS: usize = 1 << 16;
/// Tile bytes one tick may forward: 3 MB/s for a server of up to 50 players, spread out instead of in one tick.
const MAX_TILE_BYTES_PER_TICK: usize = 150 * 1024;
const MIN_TILE_REQUEST_GAP_MS: u64 = 100;
/// A client walks the network page by page, so this also bounds how fast one player can make the sidecar read.
const MIN_NETWORK_REQUEST_GAP_MS: u64 = 100;
/// Someone paging through the history by hand.
const MIN_HISTORY_REQUEST_GAP_MS: u64 = 250;
const DISC_EVERY_MS: u64 = 10_000;
/// Past this a border is the vanilla "no border": the map uses `VEXMAP_MAP_RADIUS` instead.
const NO_BORDER_HALF_SIZE: f64 = 1_000_000.0;
const DEFAULT_MAP_RADIUS: u32 = 16_384;
const DIMENSIONS: [&str; 3] = ["minecraft:overworld", "minecraft:the_nether", "minecraft:the_end"];

const KICK_NO_MOD: &str = "VexMap: this server needs the VexMap client mod.\n\
Этому серверу нужен клиентский мод VexMap.";

/// `vexmap_resourcepack::PACK_ID`. Fixed, so a new version replaces the old one on the client.
const PACK_ID: u128 = 0x5645_584d_4150_5041_434b_0000_0000_0001;
/// One report a minute at 20 TPS.
const TICK_REPORT_TICKS: usize = 1200;
const TICK_REPORT_TICKS_BENCH: usize = 200;

pub(super) fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn log(message: std::fmt::Arguments<'_>) {
    eprintln!("[vexmap] {message}");
}

// ---------------------------------------------------------------- server

type LiveJob = (String, horizon_client::mod_api::NativeChunkSnapshot);

/// The map's view of the running game. Built up over the first ticks, then a few chunks every tick.
#[derive(Default)]
struct LiveScan {
    materials: Vec<u8>,
    plan: link::LivePlan,
    next_round_ms: u64,
    worker: Option<SyncSender<LiveJob>>,
    worker_failed: bool,
}

struct Server {
    live: LiveScan,
    gate: proto::AdmissionGate,
    inbound: HashMap<u128, proto::Reassembler>,
    outbox: proto::Outbox,
    next_sample_ms: u64,
    samples: Vec<link::PlayerSample>,
    /// Who was admitted at the last sample.
    samples_admitted: Vec<u128>,
    tile_out: Vec<link::Delivery>,
    next_tile_request: HashMap<u128, u64>,
    next_network_request: HashMap<u128, u64>,
    next_history_request: HashMap<u128, u64>,
    /// Per player: when the current second began and how many edits it has seen.
    edit_limits: HashMap<u128, (u64, u32)>,
    edit_answers: Vec<link::EditDone>,
    next_disc_ms: u64,
    map_radius: u32,
    deliveries: Vec<link::Delivery>,
    pack: Option<PackOffer>,
    packs: proto::PackGate,
    tick_meter: proto::TickMeter,
    bench_admit_all: bool,
}

/// Where the resource pack is hosted. Without it the server does not ask for a pack: fine on a bench, wrong in production.
struct PackOffer {
    url: String,
    sha1: String,
}

/// `Err` says what is wrong, so a typo stops the pack loudly instead of kicking every player on a bad address.
fn parse_pack_offer(url: Option<&str>, sha1: Option<&str>) -> Result<Option<PackOffer>, String> {
    let (url, sha1) = match (url, sha1) {
        (None, None) => return Ok(None),
        (Some(url), Some(sha1)) => (url.trim(), sha1.trim()),
        _ => return Err("set both VEXMAP_PACK_URL and VEXMAP_PACK_SHA1".to_owned()),
    };
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("VEXMAP_PACK_URL is not http(s)".to_owned());
    }
    if sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return Err("VEXMAP_PACK_SHA1 is not 40 lowercase hex digits".to_owned());
    }
    Ok(Some(PackOffer { url: url.to_owned(), sha1: sha1.to_owned() }))
}

fn load_pack_offer() -> Option<PackOffer> {
    let url = std::env::var("VEXMAP_PACK_URL").ok();
    let sha1 = std::env::var("VEXMAP_PACK_SHA1").ok();
    match parse_pack_offer(url.as_deref(), sha1.as_deref()) {
        Ok(Some(offer)) => Some(offer),
        Ok(None) => {
            log(format_args!("no resource pack configured (VEXMAP_PACK_URL): players are not asked for one"));
            None
        }
        Err(error) => {
            log(format_args!("resource pack is off: {error}"));
            None
        }
    }
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);

fn with_server<R>(f: impl FnOnce(&mut Server) -> R) -> Option<R> {
    let mut guard = SERVER.lock().ok()?;
    let server = guard.get_or_insert_with(|| {
        start_sidecar_link();
        // Only for load tests with bots, which cannot answer the handshake. Never set in production.
        let bench_admit_all = std::env::var_os("VEXMAP_BENCH_ADMIT_ALL").is_some();
        if bench_admit_all {
            log(format_args!("VEXMAP_BENCH_ADMIT_ALL is set: every player is admitted without the mod"));
        }
        let window = if bench_admit_all { TICK_REPORT_TICKS_BENCH } else { TICK_REPORT_TICKS };
        Server {
            live: LiveScan::default(),
            gate: proto::AdmissionGate::new(SERVER_FEATURES.load(Ordering::Relaxed)),
            inbound: HashMap::new(),
            outbox: proto::Outbox::default(),
            next_sample_ms: 0,
            samples: Vec::new(),
            samples_admitted: Vec::new(),
            tile_out: Vec::new(),
            next_tile_request: HashMap::new(),
            next_network_request: HashMap::new(),
            next_history_request: HashMap::new(),
            edit_limits: HashMap::new(),
            edit_answers: Vec::new(),
            next_disc_ms: 0,
            map_radius: std::env::var("VEXMAP_MAP_RADIUS")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|radius| *radius > 0)
                .unwrap_or(DEFAULT_MAP_RADIUS),
            deliveries: Vec::new(),
            pack: load_pack_offer(),
            packs: proto::PackGate::default(),
            tick_meter: proto::TickMeter::new(window),
            bench_admit_all,
        }
    });
    Some(f(server))
}

/// Runs once, on the first server hook, so a player's client never starts these threads. A failure leaves the map off
/// and admission working.
fn start_sidecar_link() {
    let addr = std::env::var("VEXMAP_API_ADDR").ok();
    let feed_addr = std::env::var("VEXMAP_FEED_ADDR").ok();
    let region = std::env::var("VEXMAP_REGION").ok();
    match link::Config::parse(addr.as_deref(), region.as_deref()) {
        Ok(config) => {
            if let Err(error) = link::start(config, &SERVER_FEATURES) {
                log(format_args!("sidecar link did not start: {error}"));
            }
        }
        Err(error) => log(format_args!("sidecar link is off: {error}")),
    }
    if let Ok(config) = link::Config::parse(addr.as_deref(), region.as_deref())
        && let Err(error) = link::start_edits(config, &EDITS)
    {
        log(format_args!("route editing did not start: {error}"));
    }
    match link::FeedConfig::parse(feed_addr.as_deref(), region.as_deref()) {
        Ok(config) => {
            let boxes = link::FeedBoxes {
                players: &PLAYER_FEED,
                deliveries: &DELIVERIES,
                tile_requests: &TILE_REQUESTS,
                tiles: &TILE_DELIVERIES,
                network_wants: &NETWORK_WANTS,
                live_chunks: &LIVE_CHUNKS,
            };
            if let Err(error) = link::start_feed(config, boxes) {
                log(format_args!("player feed did not start: {error}"));
            }
        }
        Err(error) => log(format_args!("player feed is off: {error}")),
    }
}

/// At 2 Hz, on the tick thread. Only admitted players count, so a client about to be kicked never reaches the database.
/// With `realtime` off the sidecar still learns who is online, because roles need the player's record.
fn sample_players(game: &crate::game::Game<'_>, server: &mut Server, now: u64) {
    server.next_sample_ms = now + link::SAMPLE_EVERY_MS;
    server.samples.clear();
    server.samples_admitted.clear();
    for player in game.native_server_players() {
        if server.gate.features(player.uuid).is_some() {
            server.samples_admitted.push(player.uuid);
            server.samples.push(link::PlayerSample {
                uuid: player.uuid,
                name: player.name,
                dimension: player.dimension,
                position: player.position,
                yaw: player.yaw,
            });
        }
    }
    let realtime = SERVER_FEATURES.load(Ordering::Relaxed) & REALTIME != 0;
    PLAYER_FEED.publish(&mut server.samples, realtime);
}

fn send_to_player(
    game: &mut crate::game::Game<'_>,
    outbox: &mut proto::Outbox,
    player: u128,
    message: &proto::Message,
) {
    let Ok(frames) = outbox.frames(message, proto::MAX_S2C_FRAME) else { return };
    for frame in frames {
        // `Ok` is not delivery (spike S3): anything that must arrive is repeated by the protocol.
        let _ = game.native_send_custom_payload_to_player(player, proto::CHANNEL, &frame);
    }
}

fn kick_text(reason: proto::KickReason) -> String {
    match reason {
        proto::KickReason::NoHello => KICK_NO_MOD.to_owned(),
        proto::KickReason::Protocol { client } => format!(
            "VexMap: update the mod (server protocol {}, yours {client}).\n\
             Обновите мод VexMap (протокол сервера {}, ваш {client}).",
            proto::PROTOCOL,
            proto::PROTOCOL
        ),
        proto::KickReason::Pack(proto::PackFailure::Declined) => {
            "VexMap: this server needs its resource pack. Accept it to play.\n\
             Этому серверу нужен его ресурс-пак. Примите его, чтобы играть."
                .to_owned()
        }
        proto::KickReason::Pack(proto::PackFailure::Failed) => {
            "VexMap: the resource pack did not load. Try to join again.\n\
             Ресурс-пак не загрузился. Попробуйте войти снова."
                .to_owned()
        }
        proto::KickReason::Pack(proto::PackFailure::TimedOut) => {
            "VexMap: the resource pack took too long. Try to join again.\n\
             Ресурс-пак загружался слишком долго. Попробуйте войти снова."
                .to_owned()
        }
    }
}

fn kick(game: &mut crate::game::Game<'_>, outbox: &mut proto::Outbox, player: u128, reason: proto::KickReason) {
    let text = kick_text(reason);
    send_to_player(game, outbox, player, &proto::Message::Reject { reason: text.clone() });
    let queued = game.native_kick(player, &horizon_client::text::Text::literal(text));
    log(format_args!("kick {player:032x} {reason:?} queued={queued}"));
}

/// Forwards ready frames from the sidecar, which chose the addressees. Only to players whose client took `realtime`.
fn send_deliveries(game: &mut crate::game::Game<'_>, server: &mut Server) {
    if !DELIVERIES.try_take(&mut server.deliveries) {
        return;
    }
    for delivery in server.deliveries.drain(..) {
        for viewer in delivery.viewers {
            if server.gate.features(viewer).is_some_and(|features| features & REALTIME != 0) {
                let _ = game.native_send_custom_payload_to_player(viewer, proto::CHANNEL, &delivery.frame);
            }
        }
    }
}

/// Hands answers from the sidecar to the players who made the edits. A player who has left simply gets nothing.
fn send_edit_answers(game: &mut crate::game::Game<'_>, server: &mut Server) {
    EDITS.take_done(&mut server.edit_answers);
    for done in std::mem::take(&mut server.edit_answers) {
        let status = proto::EditStatus::from_http(done.http, done.replayed);
        let id = if matches!(status, proto::EditStatus::Done | proto::EditStatus::AlreadyDone) { done.id } else { 0 };
        let result = proto::Message::NetworkEditResult { token: done.token, status, id };
        send_to_player(game, &mut server.outbox, done.player, &result);
    }
}

/// An edit of the route network. The server only checks that the player took `routing` and is not flooding; whether the
/// player may edit is the sidecar's call, from the database.
fn network_edit(server: &mut Server, player: u128, token: u32, edit: proto::NetworkEdit, now: u64) {
    let admitted = server.gate.features(player).is_some_and(|features| features & ROUTING != 0);
    let region_on = SERVER_FEATURES.load(Ordering::Relaxed) & ROUTING != 0;
    if !admitted || !region_on {
        return;
    }
    let limit = server.edit_limits.entry(player).or_insert((now, 0));
    if now.saturating_sub(limit.0) >= 1000 {
        *limit = (now, 0);
    }
    limit.1 += 1;
    if limit.1 > EDITS_PER_SECOND {
        return;
    }
    let edit = match edit {
        proto::NetworkEdit::AddNode { x, z, transfer } => link::LinkEdit::AddNode { x, z, transfer },
        proto::NetworkEdit::AddLine { a, b, mode, one_way } => {
            link::LinkEdit::AddLine { a, b, mode: mode.name(), one_way }
        }
        proto::NetworkEdit::SetClosed { line, closed } => link::LinkEdit::SetClosed { line, closed },
        proto::NetworkEdit::Revert { seq } => link::LinkEdit::Revert { seq },
    };
    if !EDITS.push(link::EditJob { player, token, edit }) {
        // A full queue is told as "unavailable" on the next tick, through the same path as a dead sidecar.
        server.edit_answers.push(link::EditDone { player, token, http: 0, id: 0, replayed: false });
    }
}

/// `player_logged_in`: start the admission clock and ask the client to introduce itself.
pub(super) fn player_logged_in(game: &mut crate::game::Game<'_>, player: u128) {
    with_server(|server| {
        server.gate.join(player, now_ms());
        if server.bench_admit_all {
            server.gate.server_features = SERVER_FEATURES.load(Ordering::Relaxed);
            server.gate.hello(player, proto::PROTOCOL, FEATURES);
            return;
        }
        send_to_player(game, &mut server.outbox, player, &proto::Message::Probe);
    });
}

/// The circle of a dimension from its border: inscribed in the square (ADR-006), or the configured radius when the
/// world has no real border.
fn world_disc(bounds: [f64; 4], configured: u32) -> (i32, i32, u32) {
    let [min_x, min_z, max_x, max_z] = bounds;
    let half = ((max_x - min_x).min(max_z - min_z) / 2.0).max(1.0);
    if half >= NO_BORDER_HALF_SIZE {
        return (0, 0, configured);
    }
    (((min_x + max_x) / 2.0) as i32, ((min_z + max_z) / 2.0) as i32, half as u32)
}

fn send_discs(game: &mut crate::game::Game<'_>, server: &mut Server, player: u128) {
    for dimension in DIMENSIONS {
        let Some(bounds) = game.native_world_border(dimension) else { continue };
        let (center_x, center_z, radius) = world_disc(bounds, server.map_radius);
        let message = proto::Message::WorldDisc { dimension: dimension.to_owned(), center_x, center_z, radius };
        send_to_player(game, &mut server.outbox, player, &message);
    }
}

/// Plugin channels may lose a message and the border may move, so everyone hears the circles again now and then.
fn repeat_discs(game: &mut crate::game::Game<'_>, server: &mut Server, now: u64) {
    server.next_disc_ms = now + DISC_EVERY_MS;
    let players: Vec<u128> = server.samples_admitted.clone();
    for player in players {
        send_discs(game, server, player);
    }
}

fn send_tiles(game: &mut crate::game::Game<'_>, server: &mut Server) {
    if !TILE_DELIVERIES.try_take(&mut server.tile_out, MAX_TILE_BYTES_PER_TICK) {
        return;
    }
    for delivery in server.tile_out.drain(..) {
        for viewer in delivery.viewers {
            // Tiles and pages of the network share this path. Each was asked for by a client that took its module.
            if server.gate.features(viewer).is_some_and(|features| features & (TILES | ROUTING) != 0) {
                let _ = game.native_send_custom_payload_to_player(viewer, proto::CHANNEL, &delivery.frame);
            }
        }
    }
}

/// Hands a client's request to the feed thread without reading it. Untrusted: the sidecar validates it.
fn forward_tile_wants(server: &mut Server, player: u128, body: Vec<u8>, now: u64) {
    if server.gate.features(player).is_none_or(|features| features & TILES == 0) {
        return;
    }
    let next = server.next_tile_request.entry(player).or_insert(0);
    if now < *next {
        return;
    }
    *next = now + MIN_TILE_REQUEST_GAP_MS;
    TILE_REQUESTS.try_push(link::TileRequest { viewer: player, body });
}

/// A client asks what is new in the route network. The network is the same for everyone so far, so the sidecar only needs
/// to know who is asking for the answer's `may_edit`.
fn forward_network_want(server: &mut Server, player: u128, since: u64, render: bool, now: u64) {
    let admitted = server.gate.features(player).is_some_and(|features| features & ROUTING != 0);
    if !admitted || SERVER_FEATURES.load(Ordering::Relaxed) & ROUTING == 0 {
        return;
    }
    let next = server.next_network_request.entry(player).or_insert(0);
    if now < *next {
        return;
    }
    *next = now + MIN_NETWORK_REQUEST_GAP_MS;
    NETWORK_WANTS.try_push(link::NetworkWant { viewer: player, since, render, history: false });
}

/// A client pages through the network's history. The sidecar answers only those the database lets edit.
fn forward_network_history(server: &mut Server, player: u128, before: u64, now: u64) {
    let admitted = server.gate.features(player).is_some_and(|features| features & ROUTING != 0);
    if !admitted || SERVER_FEATURES.load(Ordering::Relaxed) & ROUTING == 0 {
        return;
    }
    let next = server.next_history_request.entry(player).or_insert(0);
    if now < *next {
        return;
    }
    *next = now + MIN_HISTORY_REQUEST_GAP_MS;
    NETWORK_WANTS.try_push(link::NetworkWant { viewer: player, since: before, render: false, history: true });
}

/// Classifies some more block states. `true` once every state is known.
fn build_block_table(game: &crate::game::Game<'_>, live: &mut LiveScan) -> bool {
    if BLOCK_MATERIALS.get().is_some() {
        return true;
    }
    for _ in 0..STATES_PER_TICK {
        let id = horizon_world::registry::BlockStateId(live.materials.len() as u32);
        match game.native_state_block(id) {
            Some(name) => live.materials.push(proto::classify(name).map_or(0, |material| material as u8)),
            None => {
                let _ = BLOCK_MATERIALS.set(std::mem::take(&mut live.materials));
                return true;
            }
        }
    }
    false
}

/// A chunk's 256 columns as the feed carries them. Off the tick thread.
fn live_columns(snapshot: &horizon_client::mod_api::NativeChunkSnapshot, dimension: &str, table: &[u8]) -> Vec<u8> {
    let [chunk_x, chunk_z] = snapshot.chunk();
    let (bottom, _) = snapshot.height();
    let survey = proto::Survey::of_dimension(dimension);
    let mut bytes = Vec::with_capacity(proto::CHUNK_COLUMN_BYTES);
    for z in chunk_z * 16..chunk_z * 16 + 16 {
        for x in chunk_x * 16..chunk_x * 16 + 16 {
            let top = snapshot.surface(x, z).unwrap_or(bottom) - 1;
            let column = proto::walk_column(survey, top, bottom, |y| {
                let state = snapshot.block_state(x, y, z)?;
                proto::Material::from_u8(*table.get(state.0 as usize)?)
            });
            bytes.extend_from_slice(&column.to_bytes());
        }
    }
    bytes
}

/// Works out what each chunk looks like and passes on only the ones that changed since it was last seen.
fn live_worker(jobs: Receiver<LiveJob>) {
    use std::hash::{Hash, Hasher};
    let Some(table) = BLOCK_MATERIALS.get() else { return };
    let mut seen: HashMap<(String, [i32; 2]), u64> = HashMap::new();
    while let Ok((dimension, snapshot)) = jobs.recv() {
        let chunk = snapshot.chunk();
        let columns = live_columns(&snapshot, &dimension, table);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        columns.hash(&mut hasher);
        let hash = hasher.finish();
        if seen.len() >= MAX_REMEMBERED_CHUNKS {
            seen.clear();
        }
        let key = (dimension.clone(), chunk);
        if seen.get(&key) == Some(&hash) {
            continue;
        }
        if LIVE_CHUNKS.try_push(link::LiveChunk { dimension, x: chunk[0], z: chunk[1], columns }) {
            seen.insert(key, hash);
        }
    }
}

fn start_live_worker(live: &mut LiveScan) -> bool {
    if live.worker.is_some() {
        return true;
    }
    if live.worker_failed {
        return false;
    }
    let (sender, receiver) = sync_channel(LIVE_JOB_QUEUE);
    match std::thread::Builder::new().name("vexmap-live".to_owned()).spawn(move || live_worker(receiver)) {
        Ok(_) => {
            live.worker = Some(sender);
            true
        }
        Err(error) => {
            live.worker_failed = true;
            log(format_args!("the live map did not start: {error}"));
            false
        }
    }
}

/// Once per tick: hands a few chunks near the players to the worker. Never reads a block here.
fn scan_live(game: &crate::game::Game<'_>, server: &mut Server, now: u64) {
    if SERVER_FEATURES.load(Ordering::Relaxed) & TILES == 0
        || !build_block_table(game, &mut server.live)
        || !start_live_worker(&mut server.live)
    {
        return;
    }
    if server.live.plan.is_empty() && now >= server.live.next_round_ms {
        server.live.next_round_ms = now + LIVE_ROUND_EVERY_MS;
        let players: Vec<(String, f64, f64)> = game
            .native_server_players()
            .into_iter()
            .filter(|player| server.gate.features(player.uuid).is_some())
            .map(|player| (player.dimension, player.position[0], player.position[2]))
            .collect();
        server.live.plan.refill(&players);
    }
    let Some(worker) = server.live.worker.clone() else { return };
    for _ in 0..LIVE_CHUNKS_PER_TICK {
        let Some((dimension, chunk)) = server.live.plan.pop() else { return };
        let Some(snapshot) = game.native_chunk_snapshot(&dimension, chunk) else { continue };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = worker.try_send((dimension, snapshot)) {
            return; // The worker is behind. The chunk comes round again.
        }
    }
}

/// `player_logged_out`: forget everything about the player.
pub(super) fn player_logged_out(player: u128) {
    with_server(|server| {
        server.next_tile_request.remove(&player);
        server.next_network_request.remove(&player);
        server.next_history_request.remove(&player);
        server.gate.leave(player);
        server.packs.leave(player);
        server.inbound.remove(&player);
        server.edit_limits.remove(&player);
    });
}

/// Once per server tick, on the tick thread. O(players waiting for admission).
pub(super) fn server_tick(game: &mut crate::game::Game<'_>) {
    let started = Instant::now();
    let now = now_ms();
    with_server(|server| {
        for action in server.gate.poll(now) {
            match action {
                proto::Action::Probe(player) => {
                    send_to_player(game, &mut server.outbox, player, &proto::Message::Probe)
                }
                proto::Action::Kick(player, reason) => kick(game, &mut server.outbox, player, reason),
            }
        }
        if now >= server.next_sample_ms {
            sample_players(game, server, now);
        }
        for player in server.packs.poll(now) {
            kick(game, &mut server.outbox, player, proto::KickReason::Pack(proto::PackFailure::TimedOut));
        }
        send_deliveries(game, server);
        send_tiles(game, server);
        send_edit_answers(game, server);
        scan_live(game, server, now);
        if now >= server.next_disc_ms {
            repeat_discs(game, server, now);
        }
        if let Some(report) = server.tick_meter.record(started.elapsed().as_nanos() as u64) {
            log(format_args!(
                "tick us: n={} p50={} p99={} max={}",
                report.ticks, report.p50_us, report.p99_us, report.max_us
            ));
        }
    });
}

/// The answer to the pack we offered. It is the only pack the mod sends, so the first version of the hook, which has no
/// pack id, is enough, and it is the one in the catalogue of named hooks.
pub(super) fn resource_pack_status(game: &mut crate::game::Game<'_>, player: u128, action: i32) {
    let Some(status) = proto::PackStatus::from_action(action) else { return };
    let now = now_ms();
    with_server(|server| {
        log(format_args!("pack {player:032x}: {status:?}"));
        if let Some(failure) = server.packs.status(player, status, now) {
            kick(game, &mut server.outbox, player, proto::KickReason::Pack(failure));
        }
    });
}

fn offer_pack(game: &mut crate::game::Game<'_>, server: &mut Server, player: u128, now: u64) {
    let Some(offer) = &server.pack else { return };
    if !server.packs.offer(player, now) {
        return;
    }
    let prompt = horizon_client::text::Text::literal("VexMap: icons for the map / иконки для карты");
    if !game.native_send_resource_pack(player, PACK_ID, &offer.url, &offer.sha1, Some(&prompt), true) {
        log(format_args!("pack was not sent to {player:032x}"));
    }
}

/// `/map` is on the server because commands are, and the screen is on the client, so the server just passes it on.
pub(super) fn register_commands(commands: &mut Vec<horizon_client::mod_api::NativeCommand>) {
    commands.push(horizon_client::mod_api::NativeCommand::new("map"));
}

pub(super) fn perform_command(
    game: &mut crate::game::Game<'_>,
    player: &horizon_client::mod_api::NativeServerPlayer,
    command: &str,
    ran: &mut bool,
) {
    if command != "map" {
        return;
    }
    *ran = true;
    with_server(|server| send_to_player(game, &mut server.outbox, player.uuid, &proto::Message::OpenMap));
}

/// A plugin channel message from a client, on the tick thread. Untrusted input.
pub(super) fn payload_from_client(game: &mut crate::game::Game<'_>, player: u128, channel: &str, data: &[u8]) {
    if channel != proto::CHANNEL {
        return;
    }
    let now = now_ms();
    with_server(|server| {
        let reassembler =
            server.inbound.entry(player).or_insert_with(|| proto::Reassembler::new(proto::Limits::default()));
        let proto::Feed::Complete { kind, body } = reassembler.feed(data, now) else { return };
        if kind == proto::Kind::TileWants as u8 {
            forward_tile_wants(server, player, body, now);
            return;
        }
        let Ok(message) = proto::Message::decode(kind, &body) else { return };
        match message {
            proto::Message::Hello { protocol, features, mod_version } => {
                server.gate.server_features = SERVER_FEATURES.load(Ordering::Relaxed);
                match server.gate.hello(player, protocol, features) {
                    proto::HelloOutcome::Welcome { features } => {
                        let region_modules = server.gate.server_features;
                        log(format_args!(
                            "admit {player:032x} mod {mod_version} features {features:#x} region {region_modules:#x}"
                        ));
                        let welcome = proto::Message::Welcome { protocol: proto::PROTOCOL, features, player };
                        send_to_player(game, &mut server.outbox, player, &welcome);
                        send_discs(game, server, player);
                        offer_pack(game, server, player, now);
                    }
                    proto::HelloOutcome::Reject(reason) => kick(game, &mut server.outbox, player, reason),
                }
            }
            proto::Message::Ping(stamp) => {
                send_to_player(game, &mut server.outbox, player, &proto::Message::Pong(stamp))
            }
            proto::Message::NetworkEdit { token, edit } => network_edit(server, player, token, edit, now),
            proto::Message::NetworkWant { since, render } => forward_network_want(server, player, since, render, now),
            proto::Message::NetworkHistoryWant { before } => forward_network_history(server, player, before, now),
            _ => {}
        }
    });
}

// ---------------------------------------------------------------- client

pub(super) struct Client {
    handshake: proto::ClientHandshake,
    inbound: proto::Reassembler,
    outbox: proto::Outbox,
    pub(super) positions: proto::PositionTables,
    pub(super) workspace: editor::Workspace,
    pub(super) navigator: routing::Navigator,
    pub(super) route_search: Option<screen::RouteSearch>,
    /// What the tiles seen so far say about the ground, for walks off the network.
    pub(super) terrain: routing::TerrainMap,
    reported_markers: usize,
    pub(super) ui: ui::Ui,
    /// This player, from `Welcome`.
    pub(super) me: Option<u128>,
    pub(super) settings_path: Option<std::path::PathBuf>,
    /// The last GUI size drawn at. Input hooks are not told it.
    pub(super) gui_size: [f32; 2],
    pub(super) open_requested: bool,
    pub(super) close_requested: bool,
    pub(super) scratch: Vec<ui::Op>,
    pub(super) tiles: ui::TileCache,
    /// Opened on first use: the map may never be drawn in a session.
    pub(super) tile_disk: Option<ui::TileDisk>,
    pub(super) tile_disk_tried: bool,
    pub(super) discs: HashMap<String, ui::Disc>,
    /// The dimension the tile cache belongs to.
    pub(super) tiles_dimension: String,
    pub(super) diagnostics: bool,
    reported_network: Option<(usize, usize, bool)>,
    pub(super) next_fps_report_ms: u64,
    pub(super) gpu_epoch: u64,
    pub(super) sent_generation: u64,
}

static CLIENT: Mutex<Option<Client>> = Mutex::new(None);

/// Read once: a missing file means a first run, which is when the map offers to pick a preset.
fn load_settings() -> (ui::Settings, bool, Option<std::path::PathBuf>) {
    let path = match crate::game::Game::native_config_dir() {
        Ok(directory) => directory.join(ui::SETTINGS_FILE),
        Err(error) => {
            log(format_args!("settings stay in memory: {error}"));
            return (ui::Settings::default(), true, None);
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let settings = ui::Settings::parse(&text);
            let offer = !settings.autotuned;
            (settings, offer, Some(path))
        }
        Err(_) => (ui::Settings::default(), true, Some(path)),
    }
}

pub(super) fn with_client<R>(f: impl FnOnce(&mut Client) -> R) -> Option<R> {
    let mut guard = CLIENT.lock().ok()?;
    let client = guard.get_or_insert_with(|| {
        let (settings, offer_autotune, settings_path) = load_settings();
        Client {
            handshake: proto::ClientHandshake::default(),
            inbound: proto::Reassembler::new(proto::Limits::default()),
            outbox: proto::Outbox::default(),
            positions: proto::PositionTables::default(),
            workspace: editor::Workspace::default(),
            navigator: routing::Navigator::default(),
            route_search: None,
            terrain: Default::default(),
            reported_markers: 0,
            ui: ui::Ui::new(settings, offer_autotune),
            me: None,
            settings_path,
            gui_size: [0.0, 0.0],
            open_requested: false,
            close_requested: false,
            scratch: Vec::new(),
            tiles: ui::TileCache::default(),
            tile_disk: None,
            tile_disk_tried: false,
            discs: HashMap::new(),
            tiles_dimension: String::new(),
            diagnostics: std::env::var_os("VEXMAP_DIAG").is_some(),
            reported_network: None,
            next_fps_report_ms: 0,
            gpu_epoch: 0,
            sent_generation: 0,
        }
    });
    Some(f(client))
}

/// The map is not drawn yet, so the log is the only sign of the table. Only when the number of markers changes.
fn report_markers(client: &mut Client) {
    let count = client.positions.marker_count();
    if client.reported_markers != count {
        client.reported_markers = count;
        log(format_args!("positions: {count} markers"));
    }
}

/// With `VEXMAP_DIAG` set, what the client holds of the route network, for the bench test. Only when it changes.
fn report_network(client: &mut Client) {
    if !client.diagnostics {
        return;
    }
    let seen =
        (client.workspace.network.len_nodes(), client.workspace.network.lines().len(), client.workspace.may_edit);
    if client.reported_network != Some(seen) {
        client.reported_network = Some(seen);
        log(format_args!("network: nodes={} lines={} may_edit={}", seen.0, seen.1, seen.2));
    }
}

fn payload_packet(channel: &str, data: Vec<u8>) -> horizon_client::mod_api::ServerboundPlayPacket {
    horizon_client::mod_api::ServerboundPlayPacket::CustomPayload(
        horizon_protocol::packets::configuration::CustomPayload { channel: channel.into(), data },
    )
}

fn send_to_server(game: &mut crate::game::Game<'_>, outbox: &mut proto::Outbox, message: &proto::Message) -> bool {
    let Ok(frames) = outbox.frames(message, proto::MAX_C2S_FRAME) else { return false };
    frames.into_iter().all(|frame| game.native_send_packet(payload_packet(proto::CHANNEL, frame)).is_ok())
}

pub(super) fn send_message(game: &mut crate::game::Game<'_>, client: &mut Client, message: &proto::Message) {
    send_to_server(game, &mut client.outbox, message);
}

/// A proxy forwards a plugin channel only once the client registered it, so `register` goes before every `Hello`.
fn introduce(game: &mut crate::game::Game<'_>, client: &mut Client) {
    let registered = game.native_send_packet(payload_packet("minecraft:register", proto::CHANNEL.as_bytes().to_vec()));
    if registered.is_err() {
        return;
    }
    let hello =
        proto::Message::Hello { protocol: proto::PROTOCOL, features: FEATURES, mod_version: MOD_VERSION.into() };
    send_to_server(game, &mut client.outbox, &hello);
}

/// Every frame on the window's game. Cheap unless a `Hello` is due (every 3 s until welcomed).
pub(super) fn frame(game: &mut crate::game::Game<'_>) {
    let now = now_ms();
    with_client(|client| {
        if client.handshake.due(now) {
            introduce(game, client);
        }
        screen::frame(game, client, now);
    });
}

/// Every play packet from the server, on the window's game.
pub(super) fn client_packet(game: &mut crate::game::Game<'_>, packet: &horizon_client::mod_api::ClientboundPlayPacket) {
    let horizon_client::mod_api::ClientboundPlayPacket::CustomPayload(payload) = packet else { return };
    if payload.channel.as_str() != proto::CHANNEL {
        return;
    }
    let now = now_ms();
    with_client(|client| {
        let proto::Feed::Complete { kind, body } = client.inbound.feed(&payload.data, now) else { return };
        let Ok(message) = proto::Message::decode(kind, &body) else { return };
        match message {
            proto::Message::Probe => {
                client.handshake.probed();
                client.handshake.due(now);
                introduce(game, client);
            }
            proto::Message::Welcome { protocol, features, player } => {
                client.handshake.welcomed();
                client.me = Some(player);
                client.workspace.welcomed(features & ROUTING != 0);
                // Another server may have another network: its roads are not the ones this route follows.
                client.navigator.clear();
                client.route_search = None;
                client.terrain = Default::default();
                client.ui.route.clear();
                client.ui.stops.clear();
                client.ui.destination = None;
                client.ui.banner = None;
                log(format_args!("welcomed: protocol {protocol}, features {features:#x}"));
            }
            proto::Message::WorldDisc { dimension, center_x, center_z, radius } if radius > 0 => {
                let disc = ui::Disc { center: [f64::from(center_x), f64::from(center_z)], radius: f64::from(radius) };
                client.discs.insert(dimension, disc);
            }
            proto::Message::Tile { dimension, id, version, pixels } => {
                screen::tile_arrived(client, &dimension, id, version, &pixels, now);
            }
            proto::Message::OpenMap => screen::open_from_server(game, client),
            proto::Message::NetworkPage(page) => {
                screen::network_page(client, &page, now);
                report_network(client);
            }
            proto::Message::NetworkEditResult { token, status, id } => {
                screen::edit_result(client, token, status, id, now)
            }
            proto::Message::NetworkHistory(history) => screen::network_history(client, &history),
            proto::Message::Reject { reason } => log(format_args!("rejected: {reason}")),
            proto::Message::Ping(stamp) => {
                send_to_server(game, &mut client.outbox, &proto::Message::Pong(stamp));
            }
            message @ (proto::Message::PositionKey(_)
            | proto::Message::PositionDelta(_)
            | proto::Message::PositionStreams(_)
            | proto::Message::PositionClusters(_)) => {
                if client.positions.apply(message) {
                    report_markers(client);
                }
            }
            _ => {}
        }
    });
}
