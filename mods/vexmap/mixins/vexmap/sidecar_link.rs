// The region server's link to the sidecar, on its own thread. The tick only ever reads one atomic, so a slow or dead
// sidecar cannot touch the game (rule 7): the last known value stays and the next poll tries again.
//
// Only `std`, because the mod cannot add dependencies to Horizon's lockfile. Also built as the `vexmap-link` crate,
// whose tests run this code against the real sidecar.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const DEFAULT_ADDR: &str = "127.0.0.1:7878";
const DEFAULT_FEED_ADDR: &str = "127.0.0.1:7879";
const GLOBAL_REGION: &str = "global";
const TIMEOUT: Duration = Duration::from_secs(2);
const POLL_EVERY: Duration = Duration::from_secs(5);
const FEED_RETRY: Duration = Duration::from_secs(2);
const FEED_POLL: Duration = Duration::from_millis(100);
/// Far above any real answer: a bound, so a confused peer cannot make this thread buffer without limit.
const MAX_RESPONSE: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub addr: SocketAddr,
    pub region: String,
    pub every: Duration,
}

impl Config {
    /// From the values of `VEXMAP_API_ADDR` and `VEXMAP_REGION`. Both end up in a request, so both are checked.
    pub fn parse(addr: Option<&str>, region: Option<&str>) -> Result<Self, String> {
        let addr = parse_loopback("VEXMAP_API_ADDR", addr.unwrap_or(DEFAULT_ADDR))?;
        Ok(Config { addr, region: parse_region(region)?, every: POLL_EVERY })
    }
}

fn parse_loopback(variable: &str, text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text.parse().map_err(|_| format!("{variable} is not an address: {text}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("{variable} must be a loopback address, got {addr}"));
    }
    Ok(addr)
}

fn parse_region(region: Option<&str>) -> Result<String, String> {
    let region = region.unwrap_or(GLOBAL_REGION);
    let valid = !region.is_empty()
        && region.len() <= 32
        && region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid {
        return Err("VEXMAP_REGION must be 1-32 characters of a-z, 0-9, - and _".to_owned());
    }
    Ok(region.to_owned())
}

/// The module mask out of a raw HTTP response from `GET /v1/features`. Only a 200 counts. The JSON is searched rather
/// than parsed: the other fields are a validated region and fixed module names, so `"mask":` cannot occur inside them.
pub fn parse_mask(response: &[u8]) -> Result<u64, String> {
    let text = std::str::from_utf8(response).map_err(|_| "response is not UTF-8".to_owned())?;
    let (head, body) = text.split_once("\r\n\r\n").ok_or("response has no body")?;
    let status = head.lines().next().unwrap_or("");
    if status.split(' ').nth(1) != Some("200") {
        return Err(format!("sidecar answered: {status}"));
    }
    let (_, rest) = body.split_once("\"mask\":").ok_or("response has no mask")?;
    let digits = rest.trim_start();
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse().map_err(|_| "mask is not a number".to_owned())
}

/// One blocking request. Never call this on the tick.
pub fn fetch_mask(config: &Config) -> Result<u64, String> {
    let failed = |error: std::io::Error| error.to_string();
    let mut stream = TcpStream::connect_timeout(&config.addr, TIMEOUT).map_err(failed)?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(failed)?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(failed)?;
    let request = format!(
        "GET /v1/features?region={} HTTP/1.1\r\nHost: {}\r\nX-VexMap-Client: region-server\r\nConnection: close\r\n\r\n",
        config.region, config.addr
    );
    stream.write_all(request.as_bytes()).map_err(failed)?;
    let mut response = Vec::new();
    stream.take(MAX_RESPONSE).read_to_end(&mut response).map_err(failed)?;
    parse_mask(&response)
}

/// Starts the thread that keeps `features` current. It logs changes of state only, not every poll.
pub fn start(config: Config, features: &'static AtomicU64) -> std::io::Result<()> {
    std::thread::Builder::new().name("vexmap-sidecar".to_owned()).spawn(move || {
        let mut last = None;
        loop {
            let result = fetch_mask(&config);
            if let Ok(mask) = result {
                features.store(mask, Ordering::Relaxed);
            }
            if last.as_ref() != Some(&result) {
                match &result {
                    Ok(mask) => eprintln!("[vexmap] sidecar: region {} modules {mask:#x}", config.region),
                    Err(error) => eprintln!("[vexmap] sidecar unavailable, keeping the last modules: {error}"),
                }
                last = Some(result);
            }
            std::thread::sleep(config.every);
        }
    })?;
    Ok(())
}

// ---------------------------------------------------------------- player feed
//
// Every batch is the full list of a region's admitted players, so there are no join or leave events to lose: whoever is
// absent has left, and a reconnect needs no replay. The tick only swaps a buffer. Encoding and the socket live here, on
// the feed thread. Loopback only, so batches are absolute values: deltas matter on the way to players, not here.

/// How often the tick samples its players: 2 Hz, inside the 1-4 Hz the map asks for.
pub const SAMPLE_EVERY_MS: u64 = 500;
/// A batch of `MAX_BATCH_PLAYERS` takes about 0.2 MiB, so this bound leaves room and still stops a confused peer.
// Enforced by the reading side: the sidecar, not the game.
#[allow(dead_code)]
pub const MAX_FEED_MESSAGE: usize = 1 << 20;
pub const MAX_BATCH_PLAYERS: usize = 4096;
const MAX_NAME: usize = 16;
const MAX_DIMENSION: usize = 64;
const MAX_REGION: usize = 32;
const MAX_DIMENSIONS: usize = 255;
/// Past any world border, and small enough that eight times it fits an `i32`.
const MAX_COORDINATE: f64 = 64_000_000.0;
const FEED_MAGIC: [u8; 2] = *b"VF";
const FEED_VERSION: u8 = 1;
const KIND_HELLO: u8 = 1;
const KIND_BATCH: u8 = 2;
const KIND_DELIVERIES: u8 = 3;
const KIND_TILE_REQUESTS: u8 = 4;
const KIND_TILE_DELIVERIES: u8 = 5;
const KIND_NETWORK_WANTS: u8 = 6;
const KIND_LIVE_CHUNKS: u8 = 7;
const FLAG_POSITIONS: u8 = 1;

/// One player as the tick saw them.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerSample {
    pub uuid: u128,
    pub name: String,
    pub dimension: String,
    pub position: [f64; 3],
    pub yaw: f32,
}

struct Slot {
    players: Vec<PlayerSample>,
    positions: bool,
    seq: u64,
    fresh: bool,
}

/// One batch deep, newest wins: the coalescing the tick needs, in a fixed amount of memory.
pub struct Mailbox {
    slot: Mutex<Slot>,
}

impl Mailbox {
    // `const` is the point: it is a `static` of the game's tick, and `Default` could not build one.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Mailbox { slot: Mutex::new(Slot { players: Vec::new(), positions: false, seq: 0, fresh: false }) }
    }

    /// For the tick: never waits. Swaps `players` in and leaves the batch it replaced for reuse. `false` means the feed
    /// thread held the slot for its own swap, and this sample is skipped: another follows in `SAMPLE_EVERY_MS`.
    pub fn publish(&self, players: &mut Vec<PlayerSample>, positions: bool) -> bool {
        let Ok(mut slot) = self.slot.try_lock() else { return false };
        std::mem::swap(&mut slot.players, players);
        slot.positions = positions;
        slot.seq += 1;
        slot.fresh = true;
        true
    }

    /// For the feed thread: the newest unread batch in `into`, as `(seq, positions)`. Gaps in `seq` are batches that
    /// were replaced before anyone read them.
    pub fn take(&self, into: &mut Vec<PlayerSample>) -> Option<(u64, bool)> {
        into.clear();
        let mut slot = self.slot.lock().ok()?;
        if !slot.fresh {
            return None;
        }
        std::mem::swap(&mut slot.players, into);
        slot.fresh = false;
        Some((slot.seq, slot.positions))
    }
}

/// `None` for a value that is not finite or not on any world. To 1/8 of a block.
fn quantize(value: f64) -> Option<i32> {
    (value.is_finite() && value.abs() <= MAX_COORDINATE).then(|| (value * 8.0).round() as i32)
}

/// To 256 steps of about 1.4 degrees, enough for a marker's arrow.
fn quantize_yaw(yaw: f32) -> u8 {
    (yaw.rem_euclid(360.0) / 360.0 * 256.0) as u32 as u8
}

fn begin_message(out: &mut Vec<u8>, kind: u8) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    out.push(kind);
    start
}

fn finish_message(out: &mut [u8], start: usize) {
    let length = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&length.to_le_bytes());
}

fn put_short_str(out: &mut Vec<u8>, text: &str) {
    out.push(text.len() as u8);
    out.extend_from_slice(text.as_bytes());
}

/// First message on every connection. `region` has passed `parse_region`, so it is short and ASCII.
pub fn encode_hello(region: &str, out: &mut Vec<u8>) {
    let start = begin_message(out, KIND_HELLO);
    out.extend_from_slice(&FEED_MAGIC);
    out.push(FEED_VERSION);
    put_short_str(out, region);
    finish_message(out, start);
}

/// A sample that cannot be represented is left out, which reads as "not on this server" until the next batch.
pub fn encode_batch(seq: u64, positions: bool, players: &[PlayerSample], out: &mut Vec<u8>) {
    let mut dimensions: Vec<&str> = Vec::new();
    let mut body = Vec::with_capacity(players.len().min(MAX_BATCH_PLAYERS) * 48);
    let mut count: u16 = 0;
    for player in players {
        if usize::from(count) == MAX_BATCH_PLAYERS || player.name.is_empty() || player.name.len() > MAX_NAME {
            continue;
        }
        let mut place = None;
        if positions {
            let dimension = match dimensions.iter().position(|known| *known == player.dimension) {
                Some(index) => Some(index),
                None if dimensions.len() < MAX_DIMENSIONS && player.dimension.len() <= MAX_DIMENSION => {
                    dimensions.push(&player.dimension);
                    Some(dimensions.len() - 1)
                }
                None => None,
            };
            let [x, y, z] = player.position.map(quantize);
            let (Some(dimension), Some(x), Some(y), Some(z)) = (dimension, x, y, z) else { continue };
            place = Some((dimension as u8, [x, y, z]));
        }
        body.extend_from_slice(&player.uuid.to_le_bytes());
        put_short_str(&mut body, &player.name);
        if let Some((dimension, coordinates)) = place {
            body.push(dimension);
            for coordinate in coordinates {
                body.extend_from_slice(&coordinate.to_le_bytes());
            }
            body.push(quantize_yaw(player.yaw));
        }
        count += 1;
    }
    let start = begin_message(out, KIND_BATCH);
    out.extend_from_slice(&seq.to_le_bytes());
    out.push(if positions { FLAG_POSITIONS } else { 0 });
    out.push(dimensions.len() as u8);
    for dimension in &dimensions {
        put_short_str(out, dimension);
    }
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&body);
    finish_message(out, start);
}

// The decoding half is used by the sidecar and tests, not by the game.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedPosition {
    pub dimension: String,
    /// In 1/8 of a block.
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// In 1/256 of a turn.
    pub yaw: u8,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedPlayer {
    pub uuid: u128,
    pub name: String,
    pub position: Option<FeedPosition>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedBatch {
    pub seq: u64,
    pub positions: bool,
    pub players: Vec<FeedPlayer>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedError {
    Truncated,
    TrailingBytes,
    BadHeader,
    BadString,
    BadDimension,
    TooManyPlayers,
}

struct FeedReader<'a> {
    bytes: &'a [u8],
}

impl<'a> FeedReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], FeedError> {
        if self.bytes.len() < count {
            return Err(FeedError::Truncated);
        }
        let (head, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], FeedError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, FeedError> {
        Ok(self.array::<1>()?[0])
    }

    #[allow(dead_code)]
    fn short_str(&mut self, max: usize) -> Result<String, FeedError> {
        let length = usize::from(self.u8()?);
        if length > max {
            return Err(FeedError::BadString);
        }
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| FeedError::BadString)
    }

    fn end(&self) -> Result<(), FeedError> {
        if self.bytes.is_empty() { Ok(()) } else { Err(FeedError::TrailingBytes) }
    }
}

/// The region named by the first message of a connection (without its length prefix).
#[allow(dead_code)]
pub fn decode_hello(body: &[u8]) -> Result<String, FeedError> {
    let mut reader = FeedReader { bytes: body };
    if reader.u8()? != KIND_HELLO || reader.array::<2>()? != FEED_MAGIC || reader.u8()? != FEED_VERSION {
        return Err(FeedError::BadHeader);
    }
    let region = reader.short_str(MAX_REGION)?;
    reader.end()?;
    Ok(region)
}

/// A batch (without its length prefix). Bad bytes are an error, never a panic.
#[allow(dead_code)]
pub fn decode_batch(body: &[u8]) -> Result<FeedBatch, FeedError> {
    let mut reader = FeedReader { bytes: body };
    if reader.u8()? != KIND_BATCH {
        return Err(FeedError::BadHeader);
    }
    let seq = u64::from_le_bytes(reader.array()?);
    let positions = reader.u8()? & FLAG_POSITIONS != 0;
    let mut dimensions = Vec::new();
    for _ in 0..reader.u8()? {
        dimensions.push(reader.short_str(MAX_DIMENSION)?);
    }
    let count = usize::from(u16::from_le_bytes(reader.array()?));
    if count > MAX_BATCH_PLAYERS {
        return Err(FeedError::TooManyPlayers);
    }
    let mut players = Vec::with_capacity(count);
    for _ in 0..count {
        let uuid = u128::from_le_bytes(reader.array()?);
        let name = reader.short_str(MAX_NAME)?;
        let position = if positions {
            let dimension = dimensions.get(usize::from(reader.u8()?)).ok_or(FeedError::BadDimension)?.clone();
            let x = i32::from_le_bytes(reader.array()?);
            let y = i32::from_le_bytes(reader.array()?);
            let z = i32::from_le_bytes(reader.array()?);
            Some(FeedPosition { dimension, x, y, z, yaw: reader.u8()? })
        } else {
            None
        };
        players.push(FeedPlayer { uuid, name, position });
    }
    reader.end()?;
    Ok(FeedBatch { seq, positions, players })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedConfig {
    pub addr: SocketAddr,
    pub region: String,
    pub retry: Duration,
    pub poll: Duration,
}

impl FeedConfig {
    /// From the values of `VEXMAP_FEED_ADDR` and `VEXMAP_REGION`.
    pub fn parse(addr: Option<&str>, region: Option<&str>) -> Result<Self, String> {
        let addr = parse_loopback("VEXMAP_FEED_ADDR", addr.unwrap_or(DEFAULT_FEED_ADDR))?;
        Ok(FeedConfig { addr, region: parse_region(region)?, retry: FEED_RETRY, poll: FEED_POLL })
    }
}

// ---------------------------------------------------------------- deliveries, sidecar to server
//
// The sidecar decides who sees what (rule 8) and hands over ready frames with their addressees. The mod reads none of it.

/// Generous for a region's channels, and still a bound on what this thread will buffer.
const MAX_DELIVERIES_MESSAGE: usize = 16 << 20;
const MAX_DELIVERIES: usize = 4096;

/// One ready frame and the players on this server who get it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub viewers: Vec<u128>,
    pub frame: Vec<u8>,
}

/// One message deep, newest wins. A position frame replaced before the tick saw it is stale anyway: deltas stand alone.
pub struct DeliveryBox {
    slot: Mutex<Vec<Delivery>>,
}

impl DeliveryBox {
    // `const` for the same reason as `Mailbox::new`.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        DeliveryBox { slot: Mutex::new(Vec::new()) }
    }

    pub fn put(&self, mut deliveries: Vec<Delivery>) {
        if let Ok(mut slot) = self.slot.lock() {
            std::mem::swap(&mut *slot, &mut deliveries);
        }
    }

    /// For the tick: never waits. `false` when there is nothing new or the feed thread holds the slot for its swap.
    pub fn try_take(&self, into: &mut Vec<Delivery>) -> bool {
        let Ok(mut slot) = self.slot.try_lock() else { return false };
        if slot.is_empty() {
            return false;
        }
        std::mem::swap(&mut *slot, into);
        true
    }
}

// ---------------------------------------------------------------- tiles
//
// Unlike positions, a tile cannot stand in for another, so these are queues and not "newest wins". Both are bounded:
// past the bound a request or a tile is dropped and the client, which never saw an answer, asks again.

/// How many requests wait for the feed thread. A tick adds at most one per player per 100 ms.
const MAX_QUEUED_TILE_REQUESTS: usize = 512;
/// How many bytes of tiles wait for the tick.
const MAX_QUEUED_TILE_BYTES: usize = 8 << 20;
const MAX_TILE_REQUEST_BODY: usize = 1024;

/// A client's raw `TileWants`. The mod does not read it: the sidecar validates untrusted input in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileRequest {
    pub viewer: u128,
    pub body: Vec<u8>,
}

pub struct TileRequestBox {
    queue: Mutex<Vec<TileRequest>>,
}

impl TileRequestBox {
    // `const` for the same reason as `Mailbox::new`.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        TileRequestBox { queue: Mutex::new(Vec::new()) }
    }

    /// For the tick: never waits. `false` means the request was dropped, and the client will repeat it.
    pub fn try_push(&self, request: TileRequest) -> bool {
        if request.body.len() > MAX_TILE_REQUEST_BODY {
            return false;
        }
        let Ok(mut queue) = self.queue.try_lock() else { return false };
        if queue.len() >= MAX_QUEUED_TILE_REQUESTS {
            return false;
        }
        queue.push(request);
        true
    }

    fn take(&self, into: &mut Vec<TileRequest>) {
        into.clear();
        if let Ok(mut queue) = self.queue.lock() {
            std::mem::swap(&mut *queue, into);
        }
    }
}

pub struct TileDeliveryBox {
    queue: Mutex<(std::collections::VecDeque<Delivery>, usize)>,
}

impl TileDeliveryBox {
    // `const` for the same reason as `Mailbox::new`.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        TileDeliveryBox { queue: Mutex::new((std::collections::VecDeque::new(), 0)) }
    }

    pub fn put(&self, deliveries: Vec<Delivery>) {
        let Ok(mut guard) = self.queue.lock() else { return };
        let (queue, bytes) = &mut *guard;
        for delivery in deliveries {
            if *bytes + delivery.frame.len() <= MAX_QUEUED_TILE_BYTES {
                *bytes += delivery.frame.len();
                queue.push_back(delivery);
            }
        }
    }

    /// For the tick: never waits, and hands over at most about `max_bytes`, so a burst is spread over several ticks.
    pub fn try_take(&self, into: &mut Vec<Delivery>, max_bytes: usize) -> bool {
        let Ok(mut guard) = self.queue.try_lock() else { return false };
        let (queue, bytes) = &mut *guard;
        let mut taken = 0;
        while taken < max_bytes {
            let Some(delivery) = queue.pop_front() else { break };
            taken += delivery.frame.len();
            *bytes -= delivery.frame.len();
            into.push(delivery);
        }
        taken > 0
    }
}

/// With its length prefix. Oversized requests are left out.
pub fn encode_tile_requests(requests: &[TileRequest], out: &mut Vec<u8>) {
    let start = begin_message(out, KIND_TILE_REQUESTS);
    let requests = &requests[..requests.len().min(MAX_QUEUED_TILE_REQUESTS)];
    out.extend_from_slice(&(requests.len() as u16).to_le_bytes());
    for request in requests {
        out.extend_from_slice(&request.viewer.to_le_bytes());
        let body = request.body.get(..MAX_TILE_REQUEST_BODY).unwrap_or(&request.body);
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(body);
    }
    finish_message(out, start);
}

/// Without the length prefix. Bad bytes are an error, never a panic.
#[allow(dead_code)]
pub fn decode_tile_requests(body: &[u8]) -> Result<Vec<TileRequest>, FeedError> {
    let mut reader = FeedReader { bytes: body };
    if reader.u8()? != KIND_TILE_REQUESTS {
        return Err(FeedError::BadHeader);
    }
    let count = usize::from(u16::from_le_bytes(reader.array()?));
    if count > MAX_QUEUED_TILE_REQUESTS {
        return Err(FeedError::TooManyPlayers);
    }
    let mut requests = Vec::with_capacity(count);
    for _ in 0..count {
        let viewer = u128::from_le_bytes(reader.array()?);
        let length = usize::from(u16::from_le_bytes(reader.array()?));
        if length > MAX_TILE_REQUEST_BODY {
            return Err(FeedError::BadString);
        }
        requests.push(TileRequest { viewer, body: reader.take(length)?.to_vec() });
    }
    reader.end()?;
    Ok(requests)
}

/// Whether a message read from the feed (without its length prefix) is a list of tile requests.
#[allow(dead_code)]
pub fn is_tile_requests(body: &[u8]) -> bool {
    body.first() == Some(&KIND_TILE_REQUESTS)
}

/// A player asks what changed in the route network after version `since`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkWant {
    pub viewer: u128,
    pub since: u64,
    /// Also asks for the whole map to be drawn at full speed.
    pub render: bool,
    /// This is a request for a page of the network's history instead, older than `since`.
    pub history: bool,
}

const MAX_QUEUED_NETWORK_WANTS: usize = 512;

/// Requests on their way to the sidecar. A full box drops the new one, and the client asks again.
pub struct NetworkWantBox {
    wants: Mutex<Vec<NetworkWant>>,
}

impl NetworkWantBox {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        NetworkWantBox { wants: Mutex::new(Vec::new()) }
    }

    pub fn try_push(&self, want: NetworkWant) -> bool {
        let Ok(mut wants) = self.wants.lock() else { return false };
        if wants.len() >= MAX_QUEUED_NETWORK_WANTS {
            return false;
        }
        wants.push(want);
        true
    }

    fn take(&self, into: &mut Vec<NetworkWant>) {
        into.clear();
        if let Ok(mut wants) = self.wants.lock() {
            std::mem::swap(&mut *wants, into);
        }
    }
}

/// With its length prefix.
pub fn encode_network_wants(wants: &[NetworkWant], out: &mut Vec<u8>) {
    let start = begin_message(out, KIND_NETWORK_WANTS);
    let wants = &wants[..wants.len().min(MAX_QUEUED_NETWORK_WANTS)];
    out.extend_from_slice(&(wants.len() as u16).to_le_bytes());
    for want in wants {
        out.extend_from_slice(&want.viewer.to_le_bytes());
        out.extend_from_slice(&want.since.to_le_bytes());
        out.push(u8::from(want.render));
        out.push(u8::from(want.history));
    }
    finish_message(out, start);
}

/// Without the length prefix. Bad bytes are an error, never a panic.
#[allow(dead_code)]
pub fn decode_network_wants(body: &[u8]) -> Result<Vec<NetworkWant>, FeedError> {
    let mut reader = FeedReader { bytes: body };
    if reader.u8()? != KIND_NETWORK_WANTS {
        return Err(FeedError::BadHeader);
    }
    let count = usize::from(u16::from_le_bytes(reader.array()?));
    if count > MAX_QUEUED_NETWORK_WANTS {
        return Err(FeedError::TooManyPlayers);
    }
    let mut wants = Vec::with_capacity(count);
    for _ in 0..count {
        wants.push(NetworkWant {
            viewer: u128::from_le_bytes(reader.array()?),
            since: u64::from_le_bytes(reader.array()?),
            render: reader.u8()? != 0,
            history: reader.u8()? != 0,
        });
    }
    reader.end()?;
    Ok(wants)
}

/// Whether a message read from the feed (without its length prefix) is a list of network requests.
#[allow(dead_code)]
pub fn is_network_wants(body: &[u8]) -> bool {
    body.first() == Some(&KIND_NETWORK_WANTS)
}

// ---------------------------------------------------------------- the map as it is right now

/// Bytes of one chunk's columns. Same as `vexmap_proto::CHUNK_COLUMN_BYTES`, which this file cannot name.
pub const LIVE_CHUNK_BYTES: usize = 1024;
const MAX_QUEUED_LIVE_CHUNKS: usize = 2048;
const MAX_LIVE_CHUNKS_PER_MESSAGE: usize = 256;
/// One round of looking around the players, in chunks. A crowd gets a smaller circle each.
const LIVE_CHUNKS_PER_ROUND: usize = 640;
const MIN_LIVE_RADIUS: i32 = 2;
const MAX_LIVE_RADIUS: i32 = 8;

/// What a chunk looks like from above, as the game has it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveChunk {
    pub dimension: String,
    pub x: i32,
    pub z: i32,
    pub columns: Vec<u8>,
}

/// Chunks on their way to the sidecar. A full box drops the new one: the worker tries it again in the next round.
pub struct LiveChunkBox {
    chunks: Mutex<Vec<LiveChunk>>,
}

impl LiveChunkBox {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        LiveChunkBox { chunks: Mutex::new(Vec::new()) }
    }

    pub fn try_push(&self, chunk: LiveChunk) -> bool {
        let Ok(mut chunks) = self.chunks.lock() else { return false };
        if chunks.len() >= MAX_QUEUED_LIVE_CHUNKS || chunk.columns.len() != LIVE_CHUNK_BYTES {
            return false;
        }
        chunks.push(chunk);
        true
    }

    fn take(&self, into: &mut Vec<LiveChunk>) {
        into.clear();
        if let Ok(mut chunks) = self.chunks.lock() {
            std::mem::swap(&mut *chunks, into);
        }
    }
}

/// With its length prefix. Chunks past the message's limit are left out, so call with at most a message's worth.
pub fn encode_live_chunks(chunks: &[LiveChunk], out: &mut Vec<u8>) {
    let start = begin_message(out, KIND_LIVE_CHUNKS);
    let chunks = &chunks[..chunks.len().min(MAX_LIVE_CHUNKS_PER_MESSAGE)];
    out.extend_from_slice(&(chunks.len() as u16).to_le_bytes());
    for chunk in chunks {
        put_short_str(out, &chunk.dimension);
        out.extend_from_slice(&chunk.x.to_le_bytes());
        out.extend_from_slice(&chunk.z.to_le_bytes());
        out.extend_from_slice(&chunk.columns);
    }
    finish_message(out, start);
}

/// Without the length prefix. Bad bytes are an error, never a panic.
#[allow(dead_code)]
pub fn decode_live_chunks(body: &[u8]) -> Result<Vec<LiveChunk>, FeedError> {
    let mut reader = FeedReader { bytes: body };
    if reader.u8()? != KIND_LIVE_CHUNKS {
        return Err(FeedError::BadHeader);
    }
    let count = usize::from(u16::from_le_bytes(reader.array()?));
    if count > MAX_LIVE_CHUNKS_PER_MESSAGE {
        return Err(FeedError::TooManyPlayers);
    }
    let mut chunks = Vec::with_capacity(count);
    for _ in 0..count {
        chunks.push(LiveChunk {
            dimension: reader.short_str(MAX_DIMENSION)?,
            x: i32::from_le_bytes(reader.array()?),
            z: i32::from_le_bytes(reader.array()?),
            columns: reader.take(LIVE_CHUNK_BYTES)?.to_vec(),
        });
    }
    reader.end()?;
    Ok(chunks)
}

/// Whether a message read from the feed (without its length prefix) is a list of live chunks.
#[allow(dead_code)]
pub fn is_live_chunks(body: &[u8]) -> bool {
    body.first() == Some(&KIND_LIVE_CHUNKS)
}

/// Which chunks to look at next: around every player, the nearest first, each once a round.
#[derive(Default)]
pub struct LivePlan {
    queue: VecDeque<(String, [i32; 2])>,
}

impl LivePlan {
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn pop(&mut self) -> Option<(String, [i32; 2])> {
        self.queue.pop_front()
    }

    /// Starts a round from where the players are now: `(dimension, x, z)` in blocks.
    pub fn refill(&mut self, players: &[(String, f64, f64)]) {
        self.queue.clear();
        if players.is_empty() {
            return;
        }
        let side = (LIVE_CHUNKS_PER_ROUND as f64 / players.len() as f64).sqrt() as i32;
        let radius = ((side - 1) / 2).clamp(MIN_LIVE_RADIUS, MAX_LIVE_RADIUS);
        let mut seen = std::collections::HashSet::new();
        for ring in 0..=radius {
            for (dimension, x, z) in players {
                let center = [(*x as i32) >> 4, (*z as i32) >> 4];
                for dx in -ring..=ring {
                    for dz in -ring..=ring {
                        if dx.abs().max(dz.abs()) != ring {
                            continue;
                        }
                        let chunk = [center[0] + dx, center[1] + dz];
                        if seen.insert((dimension.as_str(), chunk)) {
                            self.queue.push_back((dimension.clone(), chunk));
                        }
                    }
                }
            }
        }
    }
}

/// Tile frames for the region, with their length prefix.
#[allow(dead_code)]
pub fn encode_tile_deliveries(deliveries: &[Delivery], out: &mut Vec<u8>) {
    encode_deliveries_as(KIND_TILE_DELIVERIES, deliveries, out);
}

/// With its length prefix. Addressees and frames past the format's limits are left out.
#[allow(dead_code)]
pub fn encode_deliveries(deliveries: &[Delivery], out: &mut Vec<u8>) {
    encode_deliveries_as(KIND_DELIVERIES, deliveries, out);
}

fn encode_deliveries_as(kind: u8, deliveries: &[Delivery], out: &mut Vec<u8>) {
    let start = begin_message(out, kind);
    let count = deliveries.len().min(MAX_DELIVERIES);
    out.extend_from_slice(&(count as u16).to_le_bytes());
    for delivery in &deliveries[..count] {
        let viewers = &delivery.viewers[..delivery.viewers.len().min(MAX_BATCH_PLAYERS)];
        out.extend_from_slice(&(viewers.len() as u16).to_le_bytes());
        for viewer in viewers {
            out.extend_from_slice(&viewer.to_le_bytes());
        }
        out.extend_from_slice(&(delivery.frame.len() as u32).to_le_bytes());
        out.extend_from_slice(&delivery.frame);
    }
    finish_message(out, start);
}

/// Without the length prefix. Bad bytes are an error, never a panic.
pub fn decode_deliveries(body: &[u8]) -> Result<Vec<Delivery>, FeedError> {
    let mut reader = FeedReader { bytes: body };
    if !matches!(reader.u8()?, KIND_DELIVERIES | KIND_TILE_DELIVERIES) {
        return Err(FeedError::BadHeader);
    }
    let count = usize::from(u16::from_le_bytes(reader.array()?));
    if count > MAX_DELIVERIES {
        return Err(FeedError::TooManyPlayers);
    }
    let mut deliveries = Vec::with_capacity(count);
    for _ in 0..count {
        let viewer_count = usize::from(u16::from_le_bytes(reader.array()?));
        if viewer_count > MAX_BATCH_PLAYERS {
            return Err(FeedError::TooManyPlayers);
        }
        let mut viewers = Vec::with_capacity(viewer_count);
        for _ in 0..viewer_count {
            viewers.push(u128::from_le_bytes(reader.array()?));
        }
        let length = u32::from_le_bytes(reader.array()?) as usize;
        deliveries.push(Delivery { viewers, frame: reader.take(length)?.to_vec() });
    }
    reader.end()?;
    Ok(deliveries)
}

/// Reads until the socket ends or lies. A bad message ends the connection, so framing can never drift.
fn read_deliveries(mut stream: &TcpStream, deliveries: &DeliveryBox, tiles: &TileDeliveryBox) {
    let mut body = Vec::new();
    loop {
        let mut length = [0; 4];
        if stream.read_exact(&mut length).is_err() {
            return;
        }
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_DELIVERIES_MESSAGE {
            return;
        }
        body.resize(length, 0);
        if stream.read_exact(&mut body).is_err() {
            return;
        }
        match decode_deliveries(&body) {
            Ok(decoded) if body.first() == Some(&KIND_TILE_DELIVERIES) => tiles.put(decoded),
            Ok(decoded) => deliveries.put(decoded),
            Err(_) => return,
        }
    }
}

/// Where the feed thread leaves what it receives, and takes what it sends.
#[derive(Clone, Copy)]
pub struct FeedBoxes {
    pub players: &'static Mailbox,
    pub deliveries: &'static DeliveryBox,
    pub tile_requests: &'static TileRequestBox,
    pub tiles: &'static TileDeliveryBox,
    pub network_wants: &'static NetworkWantBox,
    pub live_chunks: &'static LiveChunkBox,
}

fn write_batches(mut stream: &TcpStream, config: &FeedConfig, boxes: FeedBoxes) -> std::io::Result<()> {
    let mut batch = Vec::new();
    let mut requests = Vec::new();
    let mut network_wants = Vec::new();
    let mut live_chunks = Vec::new();
    let mut out = Vec::new();
    loop {
        if let Some((seq, positions)) = boxes.players.take(&mut batch) {
            out.clear();
            encode_batch(seq, positions, &batch, &mut out);
            stream.write_all(&out)?;
        }
        boxes.tile_requests.take(&mut requests);
        if !requests.is_empty() {
            out.clear();
            encode_tile_requests(&requests, &mut out);
            stream.write_all(&out)?;
        }
        boxes.network_wants.take(&mut network_wants);
        if !network_wants.is_empty() {
            out.clear();
            encode_network_wants(&network_wants, &mut out);
            stream.write_all(&out)?;
        }
        boxes.live_chunks.take(&mut live_chunks);
        for batch in live_chunks.chunks(MAX_LIVE_CHUNKS_PER_MESSAGE) {
            out.clear();
            encode_live_chunks(batch, &mut out);
            stream.write_all(&out)?;
        }
        std::thread::sleep(config.poll);
    }
}

/// One connection, until the socket fails in either direction.
fn pump(config: &FeedConfig, boxes: FeedBoxes, connected: &mut bool) -> std::io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&config.addr, TIMEOUT)?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut hello = Vec::new();
    encode_hello(&config.region, &mut hello);
    stream.write_all(&hello)?;
    if !*connected {
        *connected = true;
        eprintln!("[vexmap] player feed: connected to {} as region {}", config.addr, config.region);
    }
    let stream = &stream;
    std::thread::scope(|scope| {
        std::thread::Builder::new().name("vexmap-feed-read".to_owned()).spawn_scoped(scope, move || {
            read_deliveries(stream, boxes.deliveries, boxes.tiles);
            // Wakes the writer: whichever direction fails first ends the connection.
            let _ = stream.shutdown(std::net::Shutdown::Both);
        })?;
        let result = write_batches(stream, config, boxes);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        result
    })
}

/// Starts the thread that carries the player list to the sidecar and frames back. While the sidecar is away only the
/// newest batch is held.
pub fn start_feed(config: FeedConfig, boxes: FeedBoxes) -> std::io::Result<()> {
    std::thread::Builder::new().name("vexmap-feed".to_owned()).spawn(move || {
        let mut connected = false;
        let mut last_error = None;
        loop {
            if let Err(error) = pump(&config, boxes, &mut connected) {
                let text = error.to_string();
                if connected || last_error.as_ref() != Some(&text) {
                    eprintln!("[vexmap] player feed lost, retrying: {text}");
                }
                connected = false;
                last_error = Some(text);
            }
            std::thread::sleep(config.retry);
        }
    })?;
    Ok(())
}

// ---------------------------------------------------------------- route network edits
//
// A player's edit travels game client -> region server -> here -> sidecar. The server never judges it: the request goes
// out with the player's UUID as `actor`, taken from the connection and not from the client, and the sidecar answers
// from the database (rule 8). The thread below waits on the sidecar so the tick never does.

use std::collections::VecDeque;
use std::sync::Condvar;

/// Edits waiting for the sidecar. Beyond this the newest are refused: a flood costs the flooder, not the game.
pub const MAX_QUEUED_EDITS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkEdit {
    AddNode {
        x: i32,
        z: i32,
        transfer: bool,
    },
    /// `mode` is the API's name: `walk`, `boat` or `train`.
    AddLine {
        a: u64,
        b: u64,
        mode: &'static str,
        one_way: bool,
    },
    SetClosed {
        line: u64,
        closed: bool,
    },
    /// Undoes the edit with this journal number.
    Revert {
        seq: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditJob {
    pub player: u128,
    pub token: u32,
    pub edit: LinkEdit,
}

/// What the sidecar said. `http` is 0 when it could not be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditDone {
    pub player: u128,
    pub token: u32,
    pub http: u16,
    /// The `seq` of the change, which is the id of a node or line it created. 0 when there was none.
    pub id: u64,
    /// The sidecar said the request was a replay.
    pub replayed: bool,
}

/// A `static` for the game: jobs in, answers out, and a thread between that waits on the sidecar.
pub struct EditQueue {
    jobs: Mutex<VecDeque<EditJob>>,
    wake: Condvar,
    done: Mutex<Vec<EditDone>>,
}

impl EditQueue {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        EditQueue { jobs: Mutex::new(VecDeque::new()), wake: Condvar::new(), done: Mutex::new(Vec::new()) }
    }

    /// For the tick. Never waits for the sidecar; `false` when the queue is full or poisoned.
    pub fn push(&self, job: EditJob) -> bool {
        let Ok(mut jobs) = self.jobs.try_lock() else { return false };
        if jobs.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        jobs.push_back(job);
        self.wake.notify_one();
        true
    }

    /// For the tick: the answers that arrived, appended to `into`. Skips this tick when the thread holds the lock.
    pub fn take_done(&self, into: &mut Vec<EditDone>) {
        if let Ok(mut done) = self.done.try_lock() {
            into.append(&mut done);
        }
    }

    fn next(&self) -> Option<EditJob> {
        let mut jobs = self.jobs.lock().ok()?;
        loop {
            if let Some(job) = jobs.pop_front() {
                return Some(job);
            }
            jobs = self.wake.wait(jobs).ok()?;
        }
    }
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, as the API reads a UUID.
pub fn uuid_text(id: u128) -> String {
    let hex = format!("{id:032x}");
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// The path and JSON body of the sidecar request for an edit made by `player`.
pub fn edit_request(player: u128, edit: &LinkEdit) -> (&'static str, String, String) {
    let actor = uuid_text(player);
    match *edit {
        LinkEdit::AddNode { x, z, transfer } => (
            "POST",
            "/v1/network/nodes".to_owned(),
            format!("{{\"x\":{x},\"z\":{z},\"transfer\":{transfer},\"actor\":\"{actor}\"}}"),
        ),
        LinkEdit::AddLine { a, b, mode, one_way } => (
            "POST",
            "/v1/network/lines".to_owned(),
            format!("{{\"a\":{a},\"b\":{b},\"mode\":\"{mode}\",\"one_way\":{one_way},\"actor\":\"{actor}\"}}"),
        ),
        LinkEdit::SetClosed { line, closed } => (
            "PUT",
            format!("/v1/network/lines/{line}/closed"),
            format!("{{\"closed\":{closed},\"actor\":\"{actor}\"}}"),
        ),
        LinkEdit::Revert { seq } => {
            ("POST", format!("/v1/network/history/{seq}/revert"), format!("{{\"actor\":\"{actor}\"}}"))
        }
    }
}

/// The HTTP status and the `seq` from a raw response. The body is the API's own fixed shape, so it is searched, not parsed.
pub fn parse_edit_reply(response: &[u8]) -> Result<(u16, u64, bool), String> {
    let text = std::str::from_utf8(response).map_err(|_| "response is not UTF-8".to_owned())?;
    let (head, body) = text.split_once("\r\n\r\n").ok_or("response has no body")?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or("response has no status")?;
    let replayed = body.contains("\"outcome\":\"replayed\"");
    let id = body.split_once("\"seq\":").map_or(0, |(_, rest)| {
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        rest[..end].parse().unwrap_or(0)
    });
    Ok((status, id, replayed))
}

/// One blocking request. `key` makes a retry of the same edit harmless.
pub fn post_edit(config: &Config, job: &EditJob, key: &str) -> Result<EditDone, String> {
    let failed = |error: std::io::Error| error.to_string();
    let (method, path, body) = edit_request(job.player, &job.edit);
    let mut stream = TcpStream::connect_timeout(&config.addr, TIMEOUT).map_err(failed)?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(failed)?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(failed)?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nX-VexMap-Client: region-server\r\nX-VexMap-Region: {}\r\n\
         Idempotency-Key: {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        config.addr,
        config.region,
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(failed)?;
    let mut response = Vec::new();
    stream.take(MAX_RESPONSE).read_to_end(&mut response).map_err(failed)?;
    let (http, id, replayed) = parse_edit_reply(&response)?;
    Ok(EditDone { player: job.player, token: job.token, http, id, replayed })
}

/// Starts the thread that sends queued edits. A sidecar that cannot be reached answers `http: 0`, so the player is told.
pub fn start_edits(config: Config, queue: &'static EditQueue) -> std::io::Result<()> {
    std::thread::Builder::new().name("vexmap-edits".to_owned()).spawn(move || {
        // Unique across restarts and players: the sidecar treats a repeated (actor, key) as the same request.
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let mut counter = 0u64;
        while let Some(job) = queue.next() {
            counter += 1;
            let key = format!("edit-{started:x}-{counter:x}");
            let done = post_edit(&config, &job, &key).unwrap_or(EditDone {
                player: job.player,
                token: job.token,
                http: 0,
                id: 0,
                replayed: false,
            });
            if let Ok(mut finished) = queue.done.lock() {
                finished.push(done);
            }
        }
    })?;
    Ok(())
}
