//! Serves the tile pyramid to region servers and keeps it current. Two threads of their own, because both block on the
//! disk: one answers requests, the other walks the world folders. The async runtime and the API never wait for either.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use vexmap_link::{Delivery, LIVE_CHUNK_BYTES, TileRequest, encode_tile_deliveries};
use vexmap_proto::{COLUMN_BYTES, Column, Kind, MAX_S2C_FRAME, Message, Outbox, TileWant};
use vexmap_tiler::store::{LiveChunk, RenderProgress, Scanner, TileStore};

use crate::feed::PositionStore;

/// The map's peak traffic budget for one player (ADR draft: 64 KB/s).
const BUDGET_BYTES_PER_SECOND: f64 = 64.0 * 1024.0;
/// Opening the map asks for a screenful at once.
const BUDGET_BURST: f64 = 512.0 * 1024.0;
const REQUEST_QUEUE: usize = 4096;
const SCAN_EVERY: Duration = Duration::from_secs(30);
/// How often chunks from the running game are drawn. A client compares versions, and a redraw costs a region's tiles.
const LIVE_EVERY: Duration = Duration::from_millis(1500);
/// Chunks waiting to be drawn, about a full round for 16 regions. Past this the newest are dropped.
const LIVE_QUEUE: usize = 8192;
/// Chunks drawn in one go, so a burst cannot hold the scan thread for long.
const LIVE_BATCH: usize = 2048;
/// Past this a chunk coordinate is not in any world we accept.
const MAX_LIVE_CHUNK: i32 = 1 << 22;
/// A full scan of a big world must not take a core from the game running beside it.
const SCAN_DUTY: f64 = 0.5;
const MAX_TRACKED_VIEWERS: usize = 16_384;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileConfig {
    /// World folders of the region servers on this host. Empty: tiles already on disk are served and nothing is redrawn.
    pub worlds: Vec<PathBuf>,
    pub store: PathBuf,
}

impl TileConfig {
    pub fn from_env(var: &impl Fn(&str) -> Option<String>) -> TileConfig {
        let worlds = var("VEXMAP_WORLD_DIRS").map_or_else(Vec::new, |value| std::env::split_paths(&value).collect());
        let store = var("VEXMAP_TILE_DIR").map_or_else(|| PathBuf::from("vexmap-tiles"), PathBuf::from);
        TileConfig { worlds, store }
    }
}

pub struct RegionRequest {
    pub region: String,
    pub request: TileRequest,
}

/// A token bucket for one player's tiles.
struct Budget {
    tokens: f64,
    updated: Instant,
}

impl Budget {
    fn full(now: Instant) -> Budget {
        Budget { tokens: BUDGET_BURST, updated: now }
    }

    fn spend(&mut self, bytes: usize, now: Instant) -> bool {
        self.tokens =
            (self.tokens + now.duration_since(self.updated).as_secs_f64() * BUDGET_BYTES_PER_SECOND).min(BUDGET_BURST);
        self.updated = now;
        if self.tokens < bytes as f64 {
            return false;
        }
        self.tokens -= bytes as f64;
        true
    }
}

/// Answers requests. Pure apart from the disk, so the rules are tested without sockets.
pub struct TileService {
    store: TileStore,
    budgets: HashMap<u128, Budget>,
    outbox: Outbox,
}

impl TileService {
    pub fn new(store: TileStore) -> TileService {
        TileService { store, budgets: HashMap::new(), outbox: Outbox::default() }
    }

    fn answer(&self, dimension: &str, want: TileWant) -> Option<Message> {
        let (version, pixels) = match self.store.read(dimension, want.id) {
            Ok(Some(tile)) if tile.version == want.version => (tile.version, Vec::new()),
            Ok(Some(tile)) => (tile.version, tile.encoded),
            Ok(None) => (0, Vec::new()),
            Err(error) => {
                tracing::warn!("tile {dimension} {:?}: {error}", want.id);
                return None;
            }
        };
        Some(Message::Tile { dimension: dimension.to_owned(), id: want.id, version, pixels })
    }

    /// Ready frames for one request. A tile over budget is left out, and the client asks for it again later. Untrusted:
    /// a body that does not parse gets no answer.
    pub fn handle(&mut self, request: &TileRequest, now: Instant) -> Vec<Delivery> {
        let Ok(Message::TileWants { dimension, wants }) = Message::decode(Kind::TileWants as u8, &request.body) else {
            return Vec::new();
        };
        if self.budgets.len() >= MAX_TRACKED_VIEWERS && !self.budgets.contains_key(&request.viewer) {
            self.budgets.clear();
        }
        let mut out = Vec::new();
        for want in wants {
            let Some(message) = self.answer(&dimension, want) else { continue };
            let Ok(frames) = self.outbox.frames(&message, MAX_S2C_FRAME) else { continue };
            let bytes = frames.iter().map(Vec::len).sum();
            let budget = self.budgets.entry(request.viewer).or_insert_with(|| Budget::full(now));
            if !budget.spend(bytes, now) {
                break;
            }
            out.extend(frames.into_iter().map(|frame| Delivery { viewers: vec![request.viewer], frame }));
        }
        out
    }
}

fn serve(mut requests: mpsc::Receiver<RegionRequest>, mut service: TileService, positions: Arc<PositionStore>) {
    while let Some(RegionRequest { region, request }) = requests.blocking_recv() {
        let deliveries = service.handle(&request, Instant::now());
        if deliveries.is_empty() {
            continue;
        }
        let mut bytes = Vec::new();
        encode_tile_deliveries(&deliveries, &mut bytes);
        if !positions.deliver_tiles(&region, bytes) {
            tracing::debug!(region, "tiles dropped: the region's queue is full or it is gone");
        }
    }
}

/// A chunk as the feed carries it, or `None` when it makes no sense.
pub fn decode_live(chunk: vexmap_link::LiveChunk) -> Option<LiveChunk> {
    if chunk.columns.len() != LIVE_CHUNK_BYTES || chunk.x.abs() > MAX_LIVE_CHUNK || chunk.z.abs() > MAX_LIVE_CHUNK {
        return None;
    }
    let columns = chunk
        .columns
        .as_chunks::<COLUMN_BYTES>()
        .0
        .iter()
        .map(|bytes| Column::from_bytes(*bytes))
        .collect::<Option<Vec<_>>>()?;
    Some(LiveChunk { dimension: chunk.dimension, x: chunk.x, z: chunk.z, columns })
}

/// A world file is outside data: a bug it trips must cost one pass, not the map for the rest of the run.
fn guarded(what: &str, work: impl FnOnce() -> std::io::Result<vexmap_tiler::store::ScanReport>) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(Ok(report)) if report.regions > 0 => tracing::debug!(?report, what, "tiles redrawn"),
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!("{what}: {error}"),
        Err(_) => tracing::error!("{what} panicked, trying again later"),
    }
}

fn scan(mut scanner: Scanner, progress: Arc<RenderProgress>) {
    let mut next_pass = Instant::now();
    loop {
        if Instant::now() >= next_pass || progress.requested.swap(false, Ordering::Relaxed) {
            guarded("tile scan", || scanner.pass());
            next_pass = Instant::now() + SCAN_EVERY;
        }
        guarded("live map", || scanner.apply_live());
        std::thread::sleep(LIVE_EVERY);
    }
}

/// Starts both threads and returns where requests and chunks from the running game go.
pub fn start(
    config: TileConfig,
    positions: Arc<PositionStore>,
    progress: Arc<RenderProgress>,
) -> std::io::Result<(mpsc::Sender<RegionRequest>, mpsc::Sender<vexmap_link::LiveChunk>)> {
    let (sender, receiver) = mpsc::channel(REQUEST_QUEUE);
    let (live_sender, mut live_receiver) = mpsc::channel::<vexmap_link::LiveChunk>(LIVE_QUEUE);
    let service = TileService::new(TileStore::new(config.store.clone()));
    let focus = Arc::clone(&positions);
    std::thread::Builder::new()
        .name("vexmap-tile-serve".to_owned())
        .spawn(move || serve(receiver, service, positions))?;
    if config.worlds.is_empty() {
        tracing::info!("VEXMAP_WORLD_DIRS is not set: only chunks from the running game are redrawn");
    }
    let scanner = Scanner::new(config.worlds, config.store, SCAN_DUTY)
        .with_progress(progress.clone())
        .with_focus(Box::new(move || {
            focus
                .online()
                .into_iter()
                .map(|player| {
                    let region = |eighths: i32| eighths.div_euclid(8 * 512); // 1/8 of a block
                    (player.marker.dimension, region(player.marker.x), region(player.marker.z))
                })
                .collect()
        }))
        .with_live(Box::new(move || {
            let mut chunks = Vec::new();
            while chunks.len() < LIVE_BATCH {
                let Ok(chunk) = live_receiver.try_recv() else { break };
                chunks.extend(decode_live(chunk));
            }
            chunks
        }));
    std::thread::Builder::new().name("vexmap-tile-scan".to_owned()).spawn(move || scan(scanner, progress))?;
    Ok((sender, live_sender))
}

#[cfg(test)]
mod tests {
    use vexmap_proto::{Feed, Limits, Reassembler, TILE_AREA, TileId};

    use super::*;

    const OVERWORLD: &str = "minecraft:overworld";
    const ID: TileId = TileId { level: 0, x: 1, z: -2 };

    fn service() -> (tempfile::TempDir, TileService) {
        let directory = tempfile::tempdir().unwrap();
        let store = TileStore::new(directory.path());
        let mut pixels = Box::new([0_u8; TILE_AREA]);
        // Worst case for the codec, so that the budget is really tested.
        for (index, pixel) in pixels.iter_mut().enumerate() {
            *pixel = (index.wrapping_mul(2_654_435_761) >> 13) as u8;
        }
        store.write(OVERWORLD, ID, 7, &pixels).unwrap();
        (directory, TileService::new(store))
    }

    fn request(version: u32, count: usize) -> TileRequest {
        let wants = vec![TileWant { id: ID, version }; count];
        TileRequest { viewer: 9, body: Message::TileWants { dimension: OVERWORLD.into(), wants }.body() }
    }

    fn message_of(delivery: &Delivery) -> Message {
        let mut reassembler = Reassembler::new(Limits::default());
        let Feed::Complete { kind, body } = reassembler.feed(&delivery.frame, 0) else { panic!("one frame") };
        Message::decode(kind, &body).unwrap()
    }

    #[test]
    fn a_live_chunk_is_checked_before_it_is_believed() {
        let good =
            vexmap_link::LiveChunk { dimension: OVERWORLD.into(), x: 1, z: -1, columns: vec![0; LIVE_CHUNK_BYTES] };
        assert_eq!(decode_live(good.clone()).unwrap().columns.len(), 256);
        assert!(decode_live(vexmap_link::LiveChunk { columns: vec![0; 10], ..good.clone() }).is_none());
        assert!(decode_live(vexmap_link::LiveChunk { x: i32::MAX, ..good.clone() }).is_none());
        assert!(decode_live(vexmap_link::LiveChunk { columns: vec![77; LIVE_CHUNK_BYTES], ..good }).is_none());
    }

    #[test]
    fn a_stored_tile_goes_only_to_who_asked() {
        let (_directory, mut service) = service();
        let out = service.handle(&request(0, 1), Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].viewers, vec![9]);
        let Message::Tile { id, version, pixels, .. } = message_of(&out[0]) else { panic!("a tile") };
        assert_eq!((id, version), (ID, 7));
        assert!(!pixels.is_empty());
    }

    #[test]
    fn a_current_copy_is_answered_without_pixels() {
        let (_directory, mut service) = service();
        let out = service.handle(&request(7, 1), Instant::now());
        let Message::Tile { version, pixels, .. } = message_of(&out[0]) else { panic!("a tile") };
        assert_eq!(version, 7);
        assert!(pixels.is_empty());
    }

    #[test]
    fn nothing_generated_is_version_zero() {
        let (_directory, mut service) = service();
        let wants = vec![TileWant { id: TileId { level: 0, x: 50, z: 50 }, version: 0 }];
        let body = Message::TileWants { dimension: OVERWORLD.into(), wants }.body();
        let out = service.handle(&TileRequest { viewer: 9, body }, Instant::now());
        let Message::Tile { version, .. } = message_of(&out[0]) else { panic!("a tile") };
        assert_eq!(version, 0);
    }

    #[test]
    fn garbage_and_foreign_paths_get_no_answer_and_no_panic() {
        let (_directory, mut service) = service();
        assert!(service.handle(&TileRequest { viewer: 1, body: vec![255; 40] }, Instant::now()).is_empty());
        let wants = vec![TileWant { id: ID, version: 0 }];
        let body = Message::TileWants { dimension: "../..:etc".into(), wants }.body();
        let out = service.handle(&TileRequest { viewer: 1, body }, Instant::now());
        let Message::Tile { version, pixels, .. } = message_of(&out[0]) else { panic!("a tile") };
        assert_eq!((version, pixels.len()), (0, 0));
    }

    #[test]
    fn the_budget_stops_a_greedy_client_and_refills() {
        let (_directory, mut service) = service();
        let start = Instant::now();
        let mut sent = 0;
        for _ in 0..4 {
            sent += service.handle(&request(0, 32), start).len();
        }
        // Each tile is about 66 KB, so the 512 KB burst holds seven of them.
        assert_eq!(sent, 7);
        assert!(service.handle(&request(0, 1), start).is_empty());
        assert_eq!(service.handle(&request(0, 1), start + Duration::from_secs(2)).len(), 1);
    }
}
