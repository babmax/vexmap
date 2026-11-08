//! What the player feed costs. `publish` runs on the game tick, so its number goes against the tick budget.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use vexmap_link::{Mailbox, PlayerSample, encode_batch};

const PLAYERS: u128 = 200;

fn batch() -> Vec<PlayerSample> {
    (0..PLAYERS)
        .map(|id| PlayerSample {
            uuid: id,
            name: format!("Player{id}"),
            dimension: "minecraft:overworld".to_owned(),
            position: [id as f64 * 3.25, 64.0, -(id as f64) * 7.5],
            yaw: 123.4,
        })
        .collect()
}

fn feed(c: &mut Criterion) {
    let mailbox = Mailbox::new();
    c.bench_function("publish_200_players", |b| {
        b.iter_batched_ref(batch, |players| mailbox.publish(black_box(players), true), BatchSize::SmallInput)
    });

    let players = batch();
    let mut out = Vec::new();
    c.bench_function("encode_200_players", |b| {
        b.iter(|| {
            out.clear();
            encode_batch(1, true, black_box(&players), &mut out);
            black_box(out.len())
        })
    });
}

criterion_group!(benches, feed);
criterion_main!(benches);
