//! Finished tiles on disk, and the scan that keeps them current. Nothing here is the source of truth: deleting the folder
//! only costs a rescan.
//!
//! Every tile sits under the visibility class `public`. Phase 11 adds other classes next to it, without moving these.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vexmap_proto::{MAX_TILE_LEVEL, TILE_AREA, TileId, decode_tile, encode_tile};

use crate::read::{Survey, read_region};
use crate::{downsample, rasterize};

const PUBLIC_CLASS: &str = "public";
const MANIFEST: &str = "scan.txt";
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

/// One region file found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Found {
    path: PathBuf,
    modified: u64,
}

type RegionKey = (String, i32, i32);

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
    pub tiles: usize,
}

/// Walks the world folders and redraws whatever changed since the last pass.
pub struct Scanner {
    worlds: Vec<PathBuf>,
    store: TileStore,
    root: PathBuf,
    done: HashMap<RegionKey, u64>,
    attempts: HashMap<RegionKey, u8>,
    /// Works this fraction of the time and sleeps the rest, so a full scan never takes a core from the game.
    duty: f64,
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
            duty: duty.clamp(0.05, 1.0),
        }
    }

    fn pause_after(&self, took: Duration) {
        std::thread::sleep(took.mul_f64((1.0 - self.duty) / self.duty));
    }

    /// One pass. Regions that fail to open keep their old tiles and are tried again next time.
    pub fn pass(&mut self) -> io::Result<ScanReport> {
        let mut report = ScanReport::default();
        let version = now_version();
        let found = scan_worlds(&self.worlds);
        let mut dirty: BTreeMap<String, BTreeSet<TileId>> = BTreeMap::new();
        for (key, file) in &found {
            if self.done.get(key) == Some(&file.modified) {
                continue;
            }
            let started = std::time::Instant::now();
            let (dimension, x, z) = (&key.0, key.1, key.2);
            let Ok((columns, read)) = read_region(&file.path, Survey::of_dimension(dimension)) else { continue };
            report.regions += 1;
            report.damaged_chunks += read.damaged;
            for (index, pixels) in rasterize(&columns).iter().enumerate() {
                let id = TileId { level: 0, x: x * 2 + (index % 2) as i32, z: z * 2 + (index / 2) as i32 };
                self.store.write(dimension, id, version, pixels)?;
                report.tiles += 1;
                if let Some(parent) = id.parent() {
                    dirty.entry(dimension.clone()).or_default().insert(parent);
                }
            }
            let attempts = self.attempts.entry(key.clone()).or_insert(0);
            *attempts += 1;
            if read.damaged == 0 || *attempts >= MAX_ATTEMPTS {
                self.done.insert(key.clone(), file.modified);
                self.attempts.remove(key);
            }
            self.pause_after(started.elapsed());
        }
        for (dimension, mut tiles) in dirty {
            for _ in 1..=MAX_TILE_LEVEL {
                let started = std::time::Instant::now();
                let mut parents = BTreeSet::new();
                for id in &tiles {
                    self.rebuild(&dimension, *id, version)?;
                    report.tiles += 1;
                    parents.extend(id.parent());
                }
                self.pause_after(started.elapsed());
                if parents.is_empty() {
                    break;
                }
                tiles = parents;
            }
        }
        save_manifest(&self.root.join(MANIFEST), &self.done)?;
        Ok(report)
    }

    fn rebuild(&self, dimension: &str, id: TileId, version: u32) -> io::Result<()> {
        let Some(children) = id.children() else { return Ok(()) };
        let mut loaded: [Option<Box<[u8; TILE_AREA]>>; 4] = [None, None, None, None];
        for (slot, child) in loaded.iter_mut().zip(children) {
            *slot = self.store.read_pixels(dimension, child)?;
        }
        let view: [Option<&[u8; TILE_AREA]>; 4] = std::array::from_fn(|index| loaded[index].as_deref());
        self.store.write(dimension, id, version, &downsample(view))
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
    use super::*;

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
    fn region_names_are_parsed_strictly() {
        assert_eq!(region_of(Path::new("a/r.-1.2.mca")), Some((-1, 2)));
        for bad in ["r.1.mca", "r.1.2.3.mca", "x.1.2.mca", "r.1.2.txt", "r.a.2.mca"] {
            assert_eq!(region_of(Path::new(bad)), None, "{bad}");
        }
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
