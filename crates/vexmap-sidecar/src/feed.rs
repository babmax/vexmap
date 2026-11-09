//! Receives each region server's player list over a local socket: who is online and, with `realtime` on, where.
//!
//! Positions live only in memory: they are a stream, not map edits (Phase 2). A player's name is the one thing that
//! reaches the database, once per connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::time::timeout;
use uuid::Uuid;
use vexmap_core::{Actor, ClientName, CommandContext, Core, RegionId, RequestId};
use vexmap_link::{
    FeedBatch, FeedPlayer, LiveChunk, MAX_FEED_MESSAGE, TileRequest, decode_batch, decode_hello, decode_live_chunks,
    decode_network_wants, decode_tile_requests, is_live_chunks, is_network_wants, is_tile_requests,
};
use vexmap_proto::Marker;

use crate::fanout::Online;
use crate::network::RegionWant;
use crate::tiles::RegionRequest;

const MAX_CONNECTIONS: usize = 256;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A server sends every 500 ms even when empty, so this much silence means it has hung.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Batches closer together than this are dropped: the rate limit of this channel, four times the real rate.
const MIN_BATCH_GAP: Duration = Duration::from_millis(50);
/// Messages of finished tiles waiting for one region. Past this the service drops them, and clients ask again.
const TILE_QUEUE: usize = 256;

/// Where requests from players go. Each kind has its own worker, and a full queue drops the request.
#[derive(Clone)]
pub struct Requests {
    pub tiles: mpsc::Sender<RegionRequest>,
    pub network: mpsc::Sender<RegionWant>,
    /// What chunks look like right now. Drawn by the tile thread.
    pub live: mpsc::Sender<LiveChunk>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlayerPosition {
    pub uuid: Uuid,
    pub name: String,
    pub dimension: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionPositions {
    pub region: String,
    /// Gaps mean the server produced batches that were replaced before they could be sent.
    pub seq: u64,
    pub age_ms: u64,
    pub players: Vec<PlayerPosition>,
}

struct Region {
    connection: u64,
    seq: u64,
    received: Instant,
    players: Vec<FeedPlayer>,
    /// Newest wins: a region that reads slowly loses stale frames instead of queueing them.
    outbox: watch::Sender<Arc<Vec<u8>>>,
    /// Tiles are not interchangeable, so these queue instead.
    tiles: mpsc::Sender<Vec<u8>>,
}

/// The newest list of each connected region. A region disappears when its server does.
#[derive(Default)]
pub struct PositionStore {
    regions: RwLock<HashMap<String, Region>>,
}

impl PositionStore {
    /// `connection` now speaks for `region`, replacing an earlier connection after a restart. Returns what to send it.
    fn claim(&self, region: &str, connection: u64) -> (watch::Receiver<Arc<Vec<u8>>>, mpsc::Receiver<Vec<u8>>) {
        let (outbox, receiver) = watch::channel(Arc::new(Vec::new()));
        let (tiles, tile_receiver) = mpsc::channel(TILE_QUEUE);
        if let Ok(mut regions) = self.regions.write() {
            let entry = Region { connection, seq: 0, received: Instant::now(), players: Vec::new(), outbox, tiles };
            regions.insert(region.to_owned(), entry);
        }
        (receiver, tile_receiver)
    }

    /// Hands finished tiles to the region's connection. `false` when they were dropped.
    pub fn deliver_tiles(&self, region: &str, message: Vec<u8>) -> bool {
        let Ok(regions) = self.regions.read() else { return false };
        regions.get(region).is_some_and(|entry| entry.tiles.try_send(message).is_ok())
    }

    /// Hands an encoded message to the region's connection.
    pub fn deliver(&self, region: &str, message: Vec<u8>) {
        if let Ok(regions) = self.regions.read()
            && let Some(entry) = regions.get(region)
        {
            entry.outbox.send_replace(Arc::new(message));
        }
    }

    /// Everyone with a known position, sorted by player. Crossing between regions a player can be on two lists for one
    /// batch, and the newer list wins.
    pub fn online(&self) -> Vec<Online> {
        let Ok(regions) = self.regions.read() else { return Vec::new() };
        let mut newest: HashMap<u128, (Instant, Online)> = HashMap::new();
        for (name, entry) in regions.iter() {
            for player in &entry.players {
                let Some(place) = &player.position else { continue };
                if newest.get(&player.uuid).is_some_and(|(received, _)| *received >= entry.received) {
                    continue;
                }
                let marker = Marker {
                    uuid: player.uuid,
                    name: player.name.clone(),
                    dimension: place.dimension.clone(),
                    x: place.x,
                    y: place.y,
                    z: place.z,
                    yaw: place.yaw,
                };
                newest.insert(player.uuid, (entry.received, Online { region: name.clone(), marker }));
            }
        }
        let mut online: Vec<Online> = newest.into_values().map(|(_, online)| online).collect();
        online.sort_by_key(|player| player.marker.uuid);
        online
    }

    /// `false` when a newer connection has taken the region over.
    fn update(&self, region: &str, connection: u64, seq: u64, players: Vec<FeedPlayer>) -> bool {
        let Ok(mut regions) = self.regions.write() else { return false };
        match regions.get_mut(region) {
            Some(entry) if entry.connection == connection => {
                entry.seq = seq;
                entry.received = Instant::now();
                entry.players = players;
                true
            }
            _ => false,
        }
    }

    fn release(&self, region: &str, connection: u64) {
        if let Ok(mut regions) = self.regions.write()
            && regions.get(region).is_some_and(|entry| entry.connection == connection)
        {
            regions.remove(region);
        }
    }

    /// One region, or all of them, in name order. A player crossing between regions can show in both for one batch.
    pub fn snapshot(&self, only: Option<&RegionId>) -> Vec<RegionPositions> {
        let Ok(regions) = self.regions.read() else { return Vec::new() };
        let mut out: Vec<RegionPositions> = regions
            .iter()
            .filter(|(name, _)| only.is_none_or(|region| region.as_str() == name.as_str()))
            .map(|(name, entry)| RegionPositions {
                region: name.clone(),
                seq: entry.seq,
                age_ms: entry.received.elapsed().as_millis() as u64,
                players: entry.players.iter().filter_map(position_of).collect(),
            })
            .collect();
        out.sort_by(|a, b| a.region.cmp(&b.region));
        out
    }
}

fn position_of(player: &FeedPlayer) -> Option<PlayerPosition> {
    let place = player.position.as_ref()?;
    Some(PlayerPosition {
        uuid: Uuid::from_u128(player.uuid),
        name: player.name.clone(),
        dimension: place.dimension.clone(),
        x: f64::from(place.x) / 8.0,
        y: f64::from(place.y) / 8.0,
        z: f64::from(place.z) / 8.0,
        yaw: f32::from(place.yaw) * 360.0 / 256.0,
    })
}

/// Accepts region servers until the listener fails. Connection ids start from the clock, so the request ids built from
/// them are not replays of an earlier run of the sidecar.
pub async fn serve(listener: TcpListener, core: Core, store: Arc<PositionStore>, requests: Requests) {
    let start = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64);
    let connections = AtomicU64::new(start);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!("feed accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::warn!("feed: too many connections");
            continue;
        };
        let connection = connections.fetch_add(1, Ordering::Relaxed);
        let (core, store, requests) = (core.clone(), store.clone(), requests.clone());
        tokio::spawn(async move {
            let _slot = slot;
            handle(stream, connection, core, store, requests).await;
        });
    }
}

async fn read_message(stream: &mut (impl AsyncRead + Unpin), buffer: &mut Vec<u8>) -> Result<(), String> {
    let length = timeout(READ_TIMEOUT, stream.read_u32_le())
        .await
        .map_err(|_| "silent for too long".to_owned())?
        .map_err(|error| error.to_string())? as usize;
    if length > MAX_FEED_MESSAGE {
        return Err(format!("message of {length} bytes"));
    }
    buffer.resize(length, 0);
    timeout(READ_TIMEOUT, stream.read_exact(buffer))
        .await
        .map_err(|_| "silent for too long".to_owned())?
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// A write that times out may have stopped mid-message, so it ends the connection rather than corrupting the framing.
async fn write_deliveries(
    mut stream: OwnedWriteHalf,
    mut outbox: watch::Receiver<Arc<Vec<u8>>>,
    mut tiles: mpsc::Receiver<Vec<u8>>,
) {
    loop {
        // Positions first: a tile may wait a moment, a marker's whole point is being current.
        let message = tokio::select! {
            biased;
            changed = outbox.changed() => {
                if changed.is_err() {
                    return;
                }
                outbox.borrow_and_update().clone()
            }
            tile = tiles.recv() => match tile {
                Some(tile) => Arc::new(tile),
                None => return,
            },
        };
        if !matches!(timeout(WRITE_TIMEOUT, stream.write_all(&message)).await, Ok(Ok(()))) {
            return;
        }
    }
}

async fn handle(stream: TcpStream, connection: u64, core: Core, store: Arc<PositionStore>, requests: Requests) {
    let (mut reader, writer) = stream.into_split();
    let mut buffer = Vec::new();
    let region = match read_message(&mut reader, &mut buffer).await.and_then(|()| hello_region(&buffer)) {
        Ok(region) => region,
        Err(error) => {
            tracing::warn!("feed: refused a connection: {error}");
            return;
        }
    };
    let (outbox, tiles) = store.claim(region.as_str(), connection);
    let writing = tokio::spawn(write_deliveries(writer, outbox, tiles));
    tracing::info!(region = region.as_str(), "feed: region connected");
    let reason = run_connection(&mut reader, &region, connection, &core, &store, &requests, &mut buffer).await;
    writing.abort();
    store.release(region.as_str(), connection);
    tracing::info!(region = region.as_str(), "feed: region gone: {reason}");
}

fn hello_region(body: &[u8]) -> Result<RegionId, String> {
    let region = decode_hello(body).map_err(|error| format!("{error:?}"))?;
    RegionId::new(&region).map_err(|error| error.to_string())
}

/// Returns why the connection ended.
async fn run_connection(
    stream: &mut (impl AsyncRead + Unpin),
    region: &RegionId,
    connection: u64,
    core: &Core,
    store: &PositionStore,
    requests: &Requests,
    buffer: &mut Vec<u8>,
) -> String {
    let mut recorded = HashMap::new();
    let mut last: Option<Instant> = None;
    loop {
        if let Err(error) = read_message(stream, buffer).await {
            return error;
        }
        if is_tile_requests(buffer) {
            match decode_tile_requests(buffer) {
                Ok(tiles) => forward_tile_requests(region, tiles, &requests.tiles),
                Err(error) => return format!("bad tile requests: {error:?}"),
            }
            continue;
        }
        if is_live_chunks(buffer) {
            match decode_live_chunks(buffer) {
                Ok(chunks) => {
                    for chunk in chunks {
                        let _ = requests.live.try_send(chunk);
                    }
                }
                Err(error) => return format!("bad live chunks: {error:?}"),
            }
            continue;
        }
        if is_network_wants(buffer) {
            match decode_network_wants(buffer) {
                Ok(wants) => {
                    for want in wants {
                        let _ = requests.network.try_send(RegionWant { region: region.as_str().to_owned(), want });
                    }
                }
                Err(error) => return format!("bad network requests: {error:?}"),
            }
            continue;
        }
        let batch = match decode_batch(buffer) {
            Ok(batch) => batch,
            Err(error) => return format!("bad batch: {error:?}"),
        };
        let now = Instant::now();
        if last.is_some_and(|at| now.duration_since(at) < MIN_BATCH_GAP) {
            continue;
        }
        last = Some(now);
        let players = batch.players.iter().filter(|player| player.position.is_some()).cloned().collect();
        if !store.update(region.as_str(), connection, batch.seq, players) {
            return "replaced by a newer connection".to_owned();
        }
        recorded = record_new_players(core, region, connection, &batch, recorded).await;
    }
}

/// Never waits: a full queue drops the request, and the client asks again.
fn forward_tile_requests(region: &RegionId, requests: Vec<TileRequest>, queue: &mpsc::Sender<RegionRequest>) {
    for request in requests {
        let _ = queue.try_send(RegionRequest { region: region.as_str().to_owned(), request });
    }
}

/// Records whoever this connection has not recorded yet. A failure is retried with the next batch.
async fn record_new_players(
    core: &Core,
    region: &RegionId,
    connection: u64,
    batch: &FeedBatch,
    mut known: HashMap<u128, String>,
) -> HashMap<u128, String> {
    let mut current = HashMap::with_capacity(batch.players.len());
    for player in &batch.players {
        if known.get(&player.uuid) == Some(&player.name) {
            if let Some(name) = known.remove(&player.uuid) {
                current.insert(player.uuid, name);
            }
            continue;
        }
        match record_player(core, region, connection, player).await {
            Ok(()) => {
                current.insert(player.uuid, player.name.clone());
            }
            Err(error) => tracing::warn!(player = %Uuid::from_u128(player.uuid), "feed: not recorded: {error}"),
        }
    }
    current
}

async fn record_player(core: &Core, region: &RegionId, connection: u64, player: &FeedPlayer) -> Result<(), String> {
    let failed = |error: vexmap_core::CoreError| error.to_string();
    // The name is part of the key: a rename seen on one connection is a new change, not a replay.
    let key = format!("{:010x}-{}-{}", connection & 0xff_ffff_ffff, Uuid::from_u128(player.uuid).simple(), player.name);
    let ctx = CommandContext {
        actor: Actor::System(ClientName::new("region-feed").map_err(failed)?),
        region: region.clone(),
        request_id: RequestId::new(&key).map_err(failed)?,
    };
    core.record_player(&ctx, Uuid::from_u128(player.uuid), &player.name).await.map_err(failed)?;
    Ok(())
}
