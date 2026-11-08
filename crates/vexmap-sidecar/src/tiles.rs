//! Serves the tile pyramid to region servers and keeps it current. Two threads of their own, because both block on the
//! disk: one answers requests, the other walks the world folders. The async runtime and the API never wait for either.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use vexmap_link::{Delivery, TileRequest, encode_tile_deliveries};
use vexmap_proto::{Kind, MAX_S2C_FRAME, Message, Outbox, TileWant};
use vexmap_tiler::store::{Scanner, TileStore};

use crate::feed::PositionStore;

/// The map's peak traffic budget for one player (ADR draft: 64 KB/s).
const BUDGET_BYTES_PER_SECOND: f64 = 64.0 * 1024.0;
/// Opening the map asks for a screenful at once.
const BUDGET_BURST: f64 = 512.0 * 1024.0;
const REQUEST_QUEUE: usize = 4096;
const SCAN_EVERY: Duration = Duration::from_secs(30);
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

fn scan(mut scanner: Scanner) {
    loop {
        // A world file is outside data: a bug it trips must cost one pass, not the map for the rest of the run.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scanner.pass())) {
            Ok(Ok(report)) if report.regions > 0 => tracing::info!(?report, "tiles redrawn"),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!("tile scan: {error}"),
            Err(_) => tracing::error!("tile scan panicked, trying again at the next pass"),
        }
        std::thread::sleep(SCAN_EVERY);
    }
}

/// Starts both threads and returns where requests go.
pub fn start(config: TileConfig, positions: Arc<PositionStore>) -> std::io::Result<mpsc::Sender<RegionRequest>> {
    let (sender, receiver) = mpsc::channel(REQUEST_QUEUE);
    let service = TileService::new(TileStore::new(config.store.clone()));
    std::thread::Builder::new()
        .name("vexmap-tile-serve".to_owned())
        .spawn(move || serve(receiver, service, positions))?;
    if config.worlds.is_empty() {
        tracing::info!("VEXMAP_WORLD_DIRS is not set: the tile store is served as it is and not redrawn");
    } else {
        let scanner = Scanner::new(config.worlds, config.store, SCAN_DUTY);
        std::thread::Builder::new().name("vexmap-tile-scan".to_owned()).spawn(move || scan(scanner))?;
    }
    Ok(sender)
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
