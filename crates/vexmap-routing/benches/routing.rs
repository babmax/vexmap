//! How long a route takes on a network as large as the data core allows: 200 000 nodes and up to 500 000 lines. The client
//! searches on its own thread, so what matters is how soon the line appears, not a frame budget.

use std::hint::black_box;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use vexmap_routing::{Graph, Ground, Mode, NoTerrain, Point, Speeds, Terrain, TerrainMap};

const SIDE: u32 = 447; // 199 809 nodes.
const SPACING: f64 = 32.0;
/// Every this many grid cells, a station, with a rail line along its row and a sea lane along its column.
const STATION_EVERY: u32 = 16;

/// A filled grid: the worst shape for A*, since nearly every node ties with its neighbour. Two lines per node on foot,
/// and rails and sea lanes on every 16th row and column, stay under the core's limit of 500 000.
fn network() -> (Graph, usize) {
    let mut graph = Graph::new();
    let mut ids = Vec::with_capacity((SIDE * SIDE) as usize);
    for z in 0..SIDE {
        for x in 0..SIDE {
            let station = x % STATION_EVERY == 0 && z % STATION_EVERY == 0;
            ids.push(graph.add_node(f64::from(x) * SPACING, f64::from(z) * SPACING, station));
        }
    }
    let id = |x: u32, z: u32| ids[(z * SIDE + x) as usize];
    let mut lines = 0;
    for z in 0..SIDE {
        for x in 0..SIDE {
            if x + 1 < SIDE {
                graph.add_line(id(x, z), id(x + 1, z), Mode::Walk, false).unwrap();
                lines += 1;
                if z % STATION_EVERY == 0 {
                    graph.add_line(id(x, z), id(x + 1, z), Mode::Train, false).unwrap();
                    lines += 1;
                }
            }
            if z + 1 < SIDE {
                graph.add_line(id(x, z), id(x, z + 1), Mode::Walk, false).unwrap();
                lines += 1;
                if x % STATION_EVERY == 0 {
                    graph.add_line(id(x, z), id(x, z + 1), Mode::Boat, false).unwrap();
                    lines += 1;
                }
            }
        }
    }
    (graph, lines)
}

fn at(x: u32, z: u32) -> Point {
    Point { x: f64::from(x) * SPACING, z: f64::from(z) * SPACING }
}

/// A lake in the way of a 30 km walk.
struct Lake;

impl Terrain for Lake {
    fn ground(&self, at: Point) -> Ground {
        if at.x.hypot(at.z) < 4000.0 { Ground::Water } else { Ground::Land }
    }
}

fn bench(c: &mut Criterion) {
    let started = Instant::now();
    let (graph, lines) = network();
    eprintln!("network: {} nodes, {lines} lines, built in {:?}", graph.node_count(), started.elapsed());
    const ALL: [Mode; 3] = [Mode::Walk, Mode::Boat, Mode::Train];
    let speeds = Speeds::default();
    let far = SIDE - 1;

    let mut group = c.benchmark_group("large_network");
    group.sample_size(10);
    group.bench_function("build_graph", |b| b.iter(|| black_box(network().1)));
    group.bench_function("snap_one_point", |b| {
        b.iter(|| graph.nearest(black_box(at(far / 2, far / 2)), Mode::Walk, 64.0))
    });
    let scenarios: [(&str, Vec<Point>, &[Mode]); 5] = [
        ("corner_to_corner_walk", vec![at(0, 0), at(far, far)], &ALL[..1]),
        ("corner_to_corner_all_modes", vec![at(0, 0), at(far, far)], &ALL),
        ("quarter_of_the_map_all_modes", vec![at(far / 2, far / 2), at(far * 3 / 4, far * 3 / 4)], &ALL),
        ("nearby_all_modes", vec![at(far / 2, far / 2), at(far / 2 + 16, far / 2 + 8)], &ALL),
        (
            "four_stops_all_modes",
            vec![at(0, 0), at(far / 2, 0), at(far / 2, far / 2), at(far, far / 2), at(far, far)],
            &ALL,
        ),
    ];
    for (name, stops, modes) in &scenarios {
        group.bench_function(*name, |b| b.iter(|| graph.route(black_box(stops), modes, &speeds).unwrap()));
    }
    group.finish();

    let mut group = c.benchmark_group("walk_off_network");
    group.sample_size(10);
    let (a, b) = (Point { x: -15_000.0, z: 40.0 }, Point { x: 15_000.0, z: 0.0 });
    let empty = Graph::new();
    group.bench_function("straight_30km", |bench| {
        bench.iter(|| empty.route_over(black_box(&[a, b]), &ALL[..1], &speeds, &NoTerrain).unwrap())
    });
    group.bench_function("around_a_lake_30km", |bench| {
        bench.iter(|| empty.route_over(black_box(&[a, b]), &ALL[..1], &speeds, &Lake).unwrap())
    });
    // A tile map as full as it gets: each search takes its own copy.
    let mut map = TerrainMap::default();
    let pixels = vec![2_u8 | (8 << 4); 256 * 256];
    for tile in 0..8192 {
        map.store(0, tile, 0, &pixels);
    }
    group.bench_function("copy_a_full_terrain_map", |bench| bench.iter(|| black_box(map.clone())));
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
