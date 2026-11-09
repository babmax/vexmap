//! Finished tiles on disk, and the scan that keeps them current. Nothing here is the source of truth: deleting the folder
//! only costs a rescan.
//!
//! Every tile sits under the visibility class `public`. Phase 11 adds other classes next to it, without moving these.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vexmap_proto::{MAX_TILE_LEVEL, TILE_AREA, TileId, decode_tile, encode_tile};

use crate::read::{Survey, read_region};
use crate::{Column, REGION_BLOCKS, RegionColumns, downsample, rasterize};

const PUBLIC_CLASS: &str = "public";
/// Which regions are already drawn. The name stands for the rules that drew them: a fix in how the world is read takes a
/// new name, so every region is drawn again. Version 2: blocks stored as a bare name stopped being read as air.
const MANIFEST: &str = "scan-2.txt";
/// A damaged region is retried this many scans, then taken as it is until its file changes again.
const MAX_ATTEMPTS: u8 = 3;

/// `minecraft:overworld` as one safe path component. Dimension names come from our own folder names, but tile requests
/// carry them too, so nothing that could leave the store directory passes.
pub fn dimension_folder(dimension: &str) -> Option<String> {
    let safe = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(&b))
            && !part.starts_with('.')
    };
    let (namespace, name) = dimension.split_once(':')?;
    (safe(namespace) && safe(name)).then(|| format!("{namespace}~{name}"))
}

/// A stored tile, ready to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredTile {
    pub version: u32,
    pub encoded: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct TileStore {
    root: PathBuf,
}

impl TileStore {
    pub fn new(root: impl Into<PathBuf>) -> TileStore {
        TileStore { root: root.into() }
    }

    fn path(&self, dimension: &str, id: TileId) -> Option<PathBuf> {
        let folder = dimension_folder(dimension)?;
        Some(
            self.root.join(PUBLIC_CLASS).join(folder).join(id.level.to_string()).join(format!("{}.{}.vxt", id.x, id.z)),
        )
    }

    /// `None` when there is no such tile.
    pub fn read(&self, dimension: &str, id: TileId) -> io::Result<Option<StoredTile>> {
        let Some(path) = self.path(dimension, id) else { return Ok(None) };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some((version, encoded)) = bytes.split_first_chunk::<4>() else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a tile file without a version"));
        };
        Ok(Some(StoredTile { version: u32::from_le_bytes(*version), encoded: encoded.to_vec() }))
    }

    fn read_pixels(&self, dimension: &str, id: TileId) -> io::Result<Option<Box<[u8; TILE_AREA]>>> {
        let Some(tile) = self.read(dimension, id)? else { return Ok(None) };
        let mut pixels = Box::new([0; TILE_AREA]);
        decode_tile(&tile.encoded, &mut pixels)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))?;
        Ok(Some(pixels))
    }

    /// Temporary file and rename, so a reader or a crash never sees half a tile.
    pub fn write(&self, dimension: &str, id: TileId, version: u32, pixels: &[u8; TILE_AREA]) -> io::Result<()> {
        let Some(path) = self.path(dimension, id) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a dimension name"));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = version.to_le_bytes().to_vec();
        encode_tile(pixels, &mut bytes);
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &path)
    }
}

impl TileStore {
    /// Writes the tile unless the stored one already shows exactly these pixels. Then the version stays, so no client
    /// downloads it again and nothing above it in the pyramid needs a redraw. Returns whether anything was written.
    pub fn write_if_changed(
        &self,
        dimension: &str,
        id: TileId,
        version: u32,
        pixels: &[u8; TILE_AREA],
    ) -> io::Result<bool> {
        // A stored tile that does not read back is simply replaced.
        if let Ok(Some(old)) = self.read_pixels(dimension, id)
            && old.as_ref() == pixels
        {
            return Ok(false);
        }
        self.write(dimension, id, version, pixels)?;
        Ok(true)
    }
}

/// One region file found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Found {
    path: PathBuf,
    modified: u64,
}

type RegionKey = (String, i32, i32);
/// Regions where players are standing now.
pub type FocusFn = Box<dyn Fn() -> Vec<RegionKey> + Send>;

/// The order to redraw regions in: those nearest a player first, so what players see is current before the far corners
/// of a big world are. Regions in a dimension nobody is in come last, in key order.
fn by_focus(todo: &mut [(&RegionKey, &Found)], focus: &[RegionKey]) {
    let distance = |key: &RegionKey| {
        focus
            .iter()
            .filter(|spot| spot.0 == key.0)
            .map(|spot| i64::from(spot.1 - key.1).pow(2) + i64::from(spot.2 - key.2).pow(2))
            .min()
            .unwrap_or(i64::MAX)
    };
    todo.sort_by_key(|(key, _)| distance(key));
}

/// `<world>/dimensions/<namespace>/<name>/region/r.X.Z.mca` in every world folder. When several servers hold the same
/// region, the file written last is the one drawn.
fn scan_worlds(worlds: &[PathBuf]) -> BTreeMap<RegionKey, Found> {
    let mut found: BTreeMap<RegionKey, Found> = BTreeMap::new();
    let folders = |path: PathBuf| -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(path) else { return Vec::new() };
        entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| Some((entry.file_name().to_str()?.to_owned(), entry.path())))
            .collect()
    };
    for world in worlds {
        for (namespace, namespace_path) in folders(world.join("dimensions")) {
            for (name, path) in folders(namespace_path) {
                let dimension = format!("{namespace}:{name}");
                if dimension_folder(&dimension).is_none() {
                    continue;
                }
                let Ok(files) = std::fs::read_dir(path.join("region")) else { continue };
                for file in files.flatten() {
                    let path = file.path();
                    let Some((x, z)) = region_of(&path) else { continue };
                    let Ok(modified) = file.metadata().and_then(|meta| meta.modified()) else { continue };
                    let modified = modified.duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64);
                    let entry = found
                        .entry((dimension.clone(), x, z))
                        .or_insert_with(|| Found { path: path.clone(), modified });
                    if modified > entry.modified {
                        *entry = Found { path, modified };
                    }
                }
            }
        }
    }
    found
}

fn region_of(path: &Path) -> Option<(i32, i32)> {
    if path.extension() != Some(OsStr::new("mca")) {
        return None;
    }
    let name = path.file_stem()?.to_str()?;
    let mut parts = name.strip_prefix("r.")?.split('.');
    let x = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((x, z))
}

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub regions: usize,
    pub damaged_chunks: usize,
    /// Tiles written.
    pub tiles: usize,
    /// Tiles redrawn that came out exactly as before: not written, so clients keep theirs.
    pub unchanged: usize,
}

/// A chunk as it is in the running game, newer than anything on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveChunk {
    pub dimension: String,
    /// Chunk coordinates.
    pub x: i32,
    pub z: i32,
    /// West to east then north to south, 256 of them.
    pub columns: Vec<Column>,
}

pub type LiveSource = Box<dyn FnMut() -> Vec<LiveChunk> + Send>;

/// The regions recently touched by live chunks. Each is a copy of its columns, so a slope across a chunk border comes out
/// right and only what changed has to arrive.
const MAX_LIVE_REGIONS: usize = 16;

struct LiveRegion {
    columns: RegionColumns,
    /// Which chunks came from the game, by position inside the region, and when, in ms since the epoch.
    chunks: HashMap<(usize, usize), u64>,
    used: u64,
}

fn copy_chunk(from: &RegionColumns, to: &mut RegionColumns, (x, z): (usize, usize)) {
    for row in 0..16 {
        let start = (z * 16 + row) * REGION_BLOCKS + x * 16;
        to.columns[start..start + 16].copy_from_slice(&from.columns[start..start + 16]);
    }
}

/// A region was just read from its file. Chunks from the game that are newer than the file are kept over it, and the
/// ones the file has caught up with are forgotten.
fn merge_live(fresh: &mut RegionColumns, live: &mut LiveRegion, file_modified: u64) {
    live.chunks.retain(|_, received| *received > file_modified);
    for chunk in live.chunks.keys() {
        copy_chunk(&live.columns, fresh, *chunk);
    }
    live.columns.clone_from(fresh);
}

/// How far drawing the world has got. Shared with whoever reports it.
#[derive(Debug, Default)]
pub struct RenderProgress {
    pub done: AtomicU32,
    pub total: AtomicU32,
    /// Someone asked for the whole map now: no pauses until it is finished.
    pub fast: AtomicBool,
    /// Start a pass without waiting for the next one.
    pub requested: AtomicBool,
}

impl RenderProgress {
    pub fn request_full(&self) {
        self.fast.store(true, Ordering::Relaxed);
        self.requested.store(true, Ordering::Relaxed);
    }
}

/// Regions drawn between saving what is done, so a restart does not start over.
const MANIFEST_EVERY: usize = 16;

/// Walks the world folders and redraws whatever changed since the last pass.
pub struct Scanner {
    worlds: Vec<PathBuf>,
    store: TileStore,
    root: PathBuf,
    done: HashMap<RegionKey, u64>,
    attempts: HashMap<RegionKey, u8>,
    /// Where players are, as `(dimension, region x, region z)`. Asked once per pass.
    focus: Option<FocusFn>,
    /// Works this fraction of the time and sleeps the rest, so a full scan never takes a core from the game.
    duty: f64,
    live: Option<LiveSource>,
    live_regions: HashMap<RegionKey, LiveRegion>,
    /// The last region files seen, to start a live region from.
    found: BTreeMap<RegionKey, Found>,
    last_version: u32,
    uses: u64,
    progress: Arc<RenderProgress>,
}

impl Scanner {
    pub fn new(worlds: Vec<PathBuf>, root: impl Into<PathBuf>, duty: f64) -> Scanner {
        let root = root.into();
        let done = load_manifest(&root.join(MANIFEST));
        Scanner {
            worlds,
            store: TileStore::new(root.clone()),
            root,
            done,
            attempts: HashMap::new(),
            focus: None,
            duty: duty.clamp(0.05, 1.0),
            live: None,
            live_regions: HashMap::new(),
            found: BTreeMap::new(),
            last_version: 0,
            uses: 0,
            progress: Arc::default(),
        }
    }

    /// Redraws regions near these places first.
    pub fn with_focus(mut self, focus: FocusFn) -> Scanner {
        self.focus = Some(focus);
        self
    }

    fn pause_after(&self, took: Duration) {
        if self.progress.fast.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(took.mul_f64((1.0 - self.duty) / self.duty));
    }

    pub fn with_progress(mut self, progress: Arc<RenderProgress>) -> Scanner {
        self.progress = progress;
        self
    }

    /// Where chunks from the running game come from.
    pub fn with_live(mut self, source: LiveSource) -> Scanner {
        self.live = Some(source);
        self
    }

    /// Strictly newer every time. A client compares versions for equality, so two drawings in one second must differ.
    fn next_version(&mut self) -> u32 {
        self.last_version = now_version().max(self.last_version.saturating_add(1));
        self.last_version
    }

    fn write_region(
        &self,
        (dimension, x, z): &RegionKey,
        columns: &RegionColumns,
        version: u32,
        dirty: &mut BTreeMap<String, BTreeSet<TileId>>,
        report: &mut ScanReport,
    ) -> io::Result<()> {
        for (index, pixels) in rasterize(columns).iter().enumerate() {
            let id = TileId { level: 0, x: x * 2 + (index % 2) as i32, z: z * 2 + (index / 2) as i32 };
            if self.store.write_if_changed(dimension, id, version, pixels)? {
                report.tiles += 1;
                if let Some(parent) = id.parent() {
                    dirty.entry(dimension.clone()).or_default().insert(parent);
                }
            } else {
                report.unchanged += 1;
            }
        }
        Ok(())
    }

    /// Redraws everything above the tiles in `dirty`, stopping as soon as a level comes out unchanged.
    fn write_parents(
        &self,
        dirty: BTreeMap<String, BTreeSet<TileId>>,
        version: u32,
        report: &mut ScanReport,
    ) -> io::Result<()> {
        for (dimension, mut tiles) in dirty {
            for _ in 1..=MAX_TILE_LEVEL {
                let started = std::time::Instant::now();
                let mut parents = BTreeSet::new();
                for id in &tiles {
                    if self.rebuild(&dimension, *id, version)? {
                        report.tiles += 1;
                        parents.extend(id.parent());
                    } else {
                        report.unchanged += 1;
                    }
                }
                self.pause_after(started.elapsed());
                if parents.is_empty() {
                    break;
                }
                tiles = parents;
            }
        }
        Ok(())
    }

    /// The live copy of a region, read from its file the first time.
    fn live_region(&mut self, key: &RegionKey) -> &mut LiveRegion {
        self.uses += 1;
        let uses = self.uses;
        let found = &self.found;
        let region = self.live_regions.entry(key.clone()).or_insert_with(|| {
            let columns = found
                .get(key)
                .and_then(|file| read_region(&file.path, Survey::of_dimension(&key.0)).ok())
                .map_or_else(RegionColumns::default, |(columns, _)| columns);
            LiveRegion { columns, chunks: HashMap::new(), used: uses }
        });
        region.used = uses;
        region
    }

    /// Draws the chunks that have arrived from the game. Costs one region's drawing per region touched.
    pub fn apply_live(&mut self) -> io::Result<ScanReport> {
        let mut report = ScanReport::default();
        let Some(source) = &mut self.live else { return Ok(report) };
        let chunks = source();
        if chunks.is_empty() {
            return Ok(report);
        }
        if self.found.is_empty() {
            self.found = scan_worlds(&self.worlds);
        }
        let received = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64);
        let mut touched = BTreeSet::new();
        for chunk in chunks {
            if chunk.columns.len() != 256 || dimension_folder(&chunk.dimension).is_none() {
                continue;
            }
            let key = (chunk.dimension.clone(), chunk.x >> 5, chunk.z >> 5);
            let inside = (chunk.x.rem_euclid(32) as usize, chunk.z.rem_euclid(32) as usize);
            let region = self.live_region(&key);
            for (row, columns) in chunk.columns.as_chunks::<16>().0.iter().enumerate() {
                let start = (inside.1 * 16 + row) * REGION_BLOCKS + inside.0 * 16;
                region.columns.columns[start..start + 16].copy_from_slice(columns);
            }
            region.chunks.insert(inside, received);
            touched.insert(key);
        }
        let version = self.next_version();
        let mut dirty = BTreeMap::new();
        for key in &touched {
            if let Some(region) = self.live_regions.get(key) {
                report.regions += 1;
                self.write_region(key, &region.columns, version, &mut dirty, &mut report)?;
            }
        }
        self.write_parents(dirty, version, &mut report)?;
        while self.live_regions.len() > MAX_LIVE_REGIONS {
            let Some(oldest) =
                self.live_regions.iter().min_by_key(|(_, region)| region.used).map(|(key, _)| key.clone())
            else {
                break;
            };
            self.live_regions.remove(&oldest);
        }
        Ok(report)
    }

    /// One pass. Regions that fail to open keep their old tiles and are tried again next time.
    pub fn pass(&mut self) -> io::Result<ScanReport> {
        let mut report = ScanReport::default();
        let found = scan_worlds(&self.worlds);
        self.found.clone_from(&found);
        let mut dirty: BTreeMap<String, BTreeSet<TileId>> = BTreeMap::new();
        let mut todo: Vec<_> = found.iter().filter(|(key, file)| self.done.get(*key) != Some(&file.modified)).collect();
        if let Some(focus) = &self.focus {
            by_focus(&mut todo, &focus());
        }
        self.progress.total.store(found.len() as u32, Ordering::Relaxed);
        self.progress.done.store((found.len() - todo.len()) as u32, Ordering::Relaxed);
        let mut since_saved = 0;
        for (key, file) in todo {
            let started = std::time::Instant::now();
            let dimension = &key.0;
            let Ok((mut columns, read)) = read_region(&file.path, Survey::of_dimension(dimension)) else { continue };
            if let Some(live) = self.live_regions.get_mut(key) {
                merge_live(&mut columns, live, file.modified);
            }
            report.regions += 1;
            report.damaged_chunks += read.damaged;
            let version = self.next_version();
            self.write_region(key, &columns, version, &mut dirty, &mut report)?;
            let attempts = self.attempts.entry(key.clone()).or_insert(0);
            *attempts += 1;
            if read.damaged == 0 || *attempts >= MAX_ATTEMPTS {
                self.done.insert(key.clone(), file.modified);
                self.attempts.remove(key);
            }
            self.pause_after(started.elapsed());
            self.progress.done.fetch_add(1, Ordering::Relaxed);
            // A long pass must not hold up what players are doing right now.
            let live = self.apply_live()?;
            report.tiles += live.tiles;
            since_saved += 1;
            if since_saved >= MANIFEST_EVERY {
                since_saved = 0;
                // Parents first: a region counted as done must have reached the whole pyramid.
                let version = self.next_version();
                self.write_parents(std::mem::take(&mut dirty), version, &mut report)?;
                save_manifest(&self.root.join(MANIFEST), &self.done)?;
            }
        }
        let version = self.next_version();
        self.write_parents(dirty, version, &mut report)?;
        save_manifest(&self.root.join(MANIFEST), &self.done)?;
        self.progress.fast.store(false, Ordering::Relaxed);
        Ok(report)
    }

    fn rebuild(&self, dimension: &str, id: TileId, version: u32) -> io::Result<bool> {
        let Some(children) = id.children() else { return Ok(false) };
        let mut loaded: [Option<Box<[u8; TILE_AREA]>>; 4] = [None, None, None, None];
        for (slot, child) in loaded.iter_mut().zip(children) {
            *slot = self.store.read_pixels(dimension, child)?;
        }
        let view: [Option<&[u8; TILE_AREA]>; 4] = std::array::from_fn(|index| loaded[index].as_deref());
        self.store.write_if_changed(dimension, id, version, &downsample(view))
    }
}

/// Seconds since 2020, at least 1: 0 means "the client has nothing".
fn now_version() -> u32 {
    const EPOCH_2020: u64 = 1_577_836_800;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    (now.saturating_sub(EPOCH_2020) as u32).max(1)
}

fn load_manifest(path: &Path) -> HashMap<RegionKey, u64> {
    let Ok(text) = std::fs::read_to_string(path) else { return HashMap::new() };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            let dimension = parts.next()?.to_owned();
            let x = parts.next()?.parse().ok()?;
            let z = parts.next()?.parse().ok()?;
            let modified = parts.next()?.parse().ok()?;
            Some(((dimension, x, z), modified))
        })
        .collect()
}

fn save_manifest(path: &Path, done: &HashMap<RegionKey, u64>) -> io::Result<()> {
    let mut lines: Vec<String> =
        done.iter().map(|((dimension, x, z), modified)| format!("{dimension} {x} {z} {modified}")).collect();
    lines.sort();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, lines.join("\n"))?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use vexmap_proto::TILE_SIZE;

    use super::*;
    use crate::{Material, material_of, shade_of};

    #[test]
    fn a_dimension_can_never_leave_the_store() {
        assert_eq!(dimension_folder("minecraft:overworld").as_deref(), Some("minecraft~overworld"));
        for bad in ["overworld", "../x:y", "a:../../etc", "a:b/c", "a:b\\c", ":b", "a:", ".:b", "A:b"] {
            assert_eq!(dimension_folder(bad), None, "{bad}");
        }
    }

    #[test]
    fn tiles_come_back_as_written() {
        let directory = tempfile::tempdir().unwrap();
        let store = TileStore::new(directory.path());
        let id = TileId { level: 2, x: -3, z: 4 };
        assert_eq!(store.read("minecraft:overworld", id).unwrap(), None);
        let mut pixels = Box::new([0; TILE_AREA]);
        pixels[77] = 9;
        store.write("minecraft:overworld", id, 5, &pixels).unwrap();
        let tile = store.read("minecraft:overworld", id).unwrap().unwrap();
        assert_eq!(tile.version, 5);
        let mut back = Box::new([0; TILE_AREA]);
        decode_tile(&tile.encoded, &mut back).unwrap();
        assert_eq!(back, pixels);
        assert_eq!(store.read("../evil:x", id).unwrap(), None);
    }

    #[test]
    fn a_redrawn_tile_that_looks_the_same_keeps_its_version() {
        let directory = tempfile::tempdir().unwrap();
        let store = TileStore::new(directory.path());
        let (world, id) = ("minecraft:overworld", TileId { level: 0, x: 1, z: 1 });
        let mut pixels = Box::new([3; TILE_AREA]);
        assert!(store.write_if_changed(world, id, 10, &pixels).unwrap());
        assert!(!store.write_if_changed(world, id, 11, &pixels).unwrap());
        assert_eq!(store.read(world, id).unwrap().unwrap().version, 10);
        pixels[5] = 4;
        assert!(store.write_if_changed(world, id, 12, &pixels).unwrap());
        assert_eq!(store.read(world, id).unwrap().unwrap().version, 12);
        // A file that does not decode is replaced, not an error.
        let path = directory.path().join("public/minecraft~overworld/0/1.1.vxt");
        std::fs::write(&path, [1, 0, 0, 0, 200]).unwrap();
        assert!(store.write_if_changed(world, id, 13, &pixels).unwrap());
    }

    #[test]
    fn regions_near_players_are_redrawn_first() {
        let found = Found { path: PathBuf::new(), modified: 1 };
        let keys: Vec<RegionKey> = vec![
            ("minecraft:overworld".into(), -9, 0),
            ("minecraft:overworld".into(), 0, 0),
            ("minecraft:overworld".into(), 5, 5),
            ("minecraft:the_nether".into(), 0, 0),
        ];
        let mut todo: Vec<_> = keys.iter().map(|key| (key, &found)).collect();
        by_focus(&mut todo, &[("minecraft:overworld".into(), 4, 4), ("minecraft:the_nether".into(), 0, 1)]);
        let order: Vec<_> = todo.iter().map(|(key, _)| (key.0.as_str(), key.1)).collect();
        assert_eq!(
            order,
            [
                ("minecraft:the_nether", 0),
                ("minecraft:overworld", 5),
                ("minecraft:overworld", 0),
                ("minecraft:overworld", -9)
            ]
        );
    }

    #[test]
    fn region_names_are_parsed_strictly() {
        assert_eq!(region_of(Path::new("a/r.-1.2.mca")), Some((-1, 2)));
        for bad in ["r.1.mca", "r.1.2.3.mca", "x.1.2.mca", "r.1.2.txt", "r.a.2.mca"] {
            assert_eq!(region_of(Path::new(bad)), None, "{bad}");
        }
    }

    const OVERWORLD: &str = "minecraft:overworld";

    fn chunk(x: i32, z: i32, material: Material) -> LiveChunk {
        let column = Column { material: Some(material), height: 64, depth: 0 };
        LiveChunk { dimension: OVERWORLD.to_owned(), x, z, columns: vec![column; 256] }
    }

    /// A scanner fed by hand, with no world on disk.
    fn live_scanner(root: &Path) -> (Scanner, std::sync::Arc<std::sync::Mutex<Vec<LiveChunk>>>) {
        let inbox = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let source = inbox.clone();
        let scanner = Scanner::new(vec![root.join("none")], root.join("tiles"), 1.0)
            .with_live(Box::new(move || std::mem::take(&mut *source.lock().unwrap())));
        (scanner, inbox)
    }

    #[test]
    fn a_live_chunk_reaches_its_tile_and_every_level_above() {
        let directory = tempfile::tempdir().unwrap();
        let (mut scanner, inbox) = live_scanner(directory.path());
        inbox.lock().unwrap().push(chunk(-1, 2, Material::Snow));
        scanner.apply_live().unwrap();
        // Chunk -1 is the last of region -1, so block -16 is in tile -1 of level 0.
        let store = TileStore::new(directory.path().join("tiles"));
        let id = TileId::containing(0, -16.0, 32.0);
        let pixels = store.read_pixels(OVERWORLD, id).unwrap().unwrap();
        let at = (32 % TILE_SIZE) * TILE_SIZE + (-16_i32).rem_euclid(TILE_SIZE as i32) as usize;
        assert_eq!(material_of(pixels[at]), Material::Snow as u8);
        assert_eq!(material_of(pixels[at - 1]), 0, "next to it the world is still unknown");
        assert!(store.read(OVERWORLD, id.parent().unwrap()).unwrap().is_some());
        assert!(store.read(OVERWORLD, TileId::containing(12, -16.0, 32.0)).unwrap().is_some());
    }

    #[test]
    fn nothing_arriving_costs_nothing_and_a_broken_chunk_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let (mut scanner, inbox) = live_scanner(directory.path());
        assert_eq!(scanner.apply_live().unwrap(), ScanReport::default());
        let mut short = chunk(0, 0, Material::Water);
        short.columns.pop();
        let mut foreign = chunk(0, 0, Material::Water);
        foreign.dimension = "../..:etc".to_owned();
        inbox.lock().unwrap().extend([short, foreign]);
        assert_eq!(scanner.apply_live().unwrap().tiles, 0);
    }

    #[test]
    fn every_change_gets_a_newer_version_even_within_a_second() {
        let directory = tempfile::tempdir().unwrap();
        let (mut scanner, inbox) = live_scanner(directory.path());
        let store = TileStore::new(directory.path().join("tiles"));
        let id = TileId { level: 0, x: 0, z: 0 };
        inbox.lock().unwrap().push(chunk(0, 0, Material::Grass));
        scanner.apply_live().unwrap();
        let first = store.read(OVERWORLD, id).unwrap().unwrap().version;
        inbox.lock().unwrap().push(chunk(0, 0, Material::Sand));
        scanner.apply_live().unwrap();
        assert!(store.read(OVERWORLD, id).unwrap().unwrap().version > first);
    }

    #[test]
    fn the_same_chunk_again_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let (mut scanner, inbox) = live_scanner(directory.path());
        inbox.lock().unwrap().push(chunk(3, 3, Material::Dirt));
        scanner.apply_live().unwrap();
        inbox.lock().unwrap().push(chunk(3, 3, Material::Dirt));
        assert_eq!(scanner.apply_live().unwrap().tiles, 0);
    }

    #[test]
    fn a_slope_across_a_chunk_border_is_drawn_from_both_sides() {
        let directory = tempfile::tempdir().unwrap();
        let (mut scanner, inbox) = live_scanner(directory.path());
        let mut high = chunk(1, 0, Material::Stone);
        high.columns.iter_mut().for_each(|column| column.height = 80);
        inbox.lock().unwrap().extend([chunk(0, 0, Material::Stone), high]);
        scanner.apply_live().unwrap();
        let store = TileStore::new(directory.path().join("tiles"));
        let pixels = store.read_pixels(OVERWORLD, TileId { level: 0, x: 0, z: 0 }).unwrap().unwrap();
        assert_ne!(shade_of(pixels[16]), shade_of(pixels[7]), "the first column of chunk 1 sees the drop in chunk 0");
    }

    #[test]
    fn a_file_replaces_what_it_has_caught_up_with_and_nothing_newer() {
        let fill = |material| {
            let mut region = RegionColumns::default();
            region.columns.fill(Column { material: Some(material), height: 64, depth: 0 });
            region
        };
        let mut live = LiveRegion { columns: fill(Material::Snow), chunks: HashMap::new(), used: 0 };
        live.chunks.insert((0, 0), 1000); // Older than the file.
        live.chunks.insert((5, 7), 3000); // Newer than the file.
        let mut fresh = fill(Material::Grass);
        merge_live(&mut fresh, &mut live, 2000);
        assert_eq!(fresh.columns[0].material, Some(Material::Grass));
        let inside = (7 * 16) * REGION_BLOCKS + 5 * 16;
        assert_eq!(fresh.columns[inside].material, Some(Material::Snow));
        assert_eq!(live.chunks.len(), 1, "the file has caught up with the other");
        assert_eq!(live.columns.columns[0].material, Some(Material::Grass));
    }

    #[test]
    fn a_pass_reports_how_far_it_got_and_ends_a_full_drawing() {
        let directory = tempfile::tempdir().unwrap();
        let progress = Arc::new(RenderProgress::default());
        let mut scanner = Scanner::new(vec![directory.path().join("none")], directory.path().join("tiles"), 1.0)
            .with_progress(progress.clone());
        progress.request_full();
        scanner.pass().unwrap();
        assert!(!progress.fast.load(Ordering::Relaxed), "nothing is left to hurry for");
        assert_eq!(progress.total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_manifest_survives_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(MANIFEST);
        let done = HashMap::from([(("minecraft:overworld".to_owned(), -1, 2), 77)]);
        save_manifest(&path, &done).unwrap();
        assert_eq!(load_manifest(&path), done);
    }

    #[test]
    fn an_empty_world_is_a_quiet_pass() {
        let directory = tempfile::tempdir().unwrap();
        let mut scanner = Scanner::new(vec![directory.path().join("none")], directory.path().join("tiles"), 1.0);
        assert_eq!(scanner.pass().unwrap(), ScanReport::default());
    }
}
