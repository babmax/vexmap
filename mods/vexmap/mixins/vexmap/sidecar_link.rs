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

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedError {
    Truncated,
    TrailingBytes,
    BadHeader,
    BadString,
    BadDimension,
    TooManyPlayers,
}

#[allow(dead_code)]
struct FeedReader<'a> {
    bytes: &'a [u8],
}

#[allow(dead_code)]
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

/// One connection, until the socket fails.
fn pump(config: &FeedConfig, mailbox: &Mailbox, connected: &mut bool) -> std::io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&config.addr, TIMEOUT)?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut out = Vec::new();
    encode_hello(&config.region, &mut out);
    stream.write_all(&out)?;
    if !*connected {
        *connected = true;
        eprintln!("[vexmap] player feed: connected to {} as region {}", config.addr, config.region);
    }
    let mut batch = Vec::new();
    loop {
        if let Some((seq, positions)) = mailbox.take(&mut batch) {
            out.clear();
            encode_batch(seq, positions, &batch, &mut out);
            stream.write_all(&out)?;
        }
        std::thread::sleep(config.poll);
    }
}

/// Starts the thread that carries `mailbox` to the sidecar. While the sidecar is away only the newest batch is held.
pub fn start_feed(config: FeedConfig, mailbox: &'static Mailbox) -> std::io::Result<()> {
    std::thread::Builder::new().name("vexmap-feed".to_owned()).spawn(move || {
        let mut connected = false;
        let mut last_error = None;
        loop {
            if let Err(error) = pump(&config, mailbox, &mut connected) {
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
