//! What position fan-out costs at the target of 5000 players, and what a client pays to apply a delta.

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use vexmap_core::{CountryId, Role};
use vexmap_proto::{ChannelEncoder, Marker, PositionTable};
use vexmap_sidecar::fanout::{Fanout, Online, Viewer};

const PLAYERS: u128 = 5000;
const COUNTRIES: u128 = 25;
const REGIONS: u128 = 100;
const MARKERS_ON_A_CLIENT: u128 = 1000;

fn marker(uuid: u128, step: i32) -> Marker {
    Marker {
        uuid,
        name: format!("Player{uuid}"),
        dimension: "minecraft:overworld".to_owned(),
        x: uuid as i32 * 40 + step * 16,
        y: 512,
        z: -(uuid as i32) * 24 + step * 8,
        yaw: step as u8,
    }
}

fn online(step: i32) -> Vec<Online> {
    (0..PLAYERS).map(|uuid| Online { region: format!("r{}", uuid % REGIONS), marker: marker(uuid, step) }).collect()
}

fn directory() -> HashMap<u128, Viewer> {
    (0..PLAYERS)
        .map(|uuid| {
            let country = CountryId::new(&format!("c{}", uuid % COUNTRIES)).ok();
            (uuid, Viewer { role: Role::Player, country })
        })
        .collect()
}

fn benches(c: &mut Criterion) {
    let directory = directory();
    let mut fanout = Fanout::default();
    fanout.step(&online(0), &directory);
    let moved = online(1);
    let moved_again = online(2);
    let mut flip = false;
    // Every player moves every step: the worst case for deltas.
    c.bench_function("fanout_5000_players_25_countries", |b| {
        b.iter(|| {
            flip = !flip;
            black_box(fanout.step(if flip { &moved } else { &moved_again }, &directory).len())
        })
    });

    let mut encoder = ChannelEncoder::default();
    let mut table = PositionTable::default();
    table.apply_key(encoder.key_frame(1, (0..MARKERS_ON_A_CLIENT).map(|uuid| marker(uuid, 0)).collect()));
    let (delta, _) = encoder.delta((0..MARKERS_ON_A_CLIENT).map(|uuid| marker(uuid, 1)).collect());
    let mut delta = delta.clone();
    c.bench_function("client_apply_delta_1000_markers", |b| {
        b.iter(|| {
            delta.seq += 1;
            black_box(table.apply_delta(black_box(&delta)))
        })
    });
}

criterion_group!(group, benches);
criterion_main!(group);
