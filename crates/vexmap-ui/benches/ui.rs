use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use vexmap_ui::{Group, Pin, Player, Settings, Ui};

const RECT: [f32; 2] = [960.0, 540.0];

fn world(markers: usize) -> (Vec<Pin<'static>>, Vec<Group<'static>>, Player) {
    let pins = (0..markers)
        .map(|index| {
            let angle = index as f64 * 2.399_963;
            let radius = (index as f64).sqrt() * 8.0;
            Pin {
                uuid: index as u128 + 1,
                name: "PlayerName1234",
                dimension: "minecraft:overworld",
                x: angle.cos() * radius,
                z: angle.sin() * radius,
            }
        })
        .collect();
    let groups = (0..32)
        .map(|index| Group { dimension: "minecraft:overworld", x: index as f64 * 40.0, z: 0.0, count: 12 })
        .collect();
    let player = Player { x: 0.0, y: 64.0, z: 0.0, yaw: 30.0, dimension: "minecraft:overworld".to_owned() };
    (pins, groups, player)
}

fn benches(c: &mut Criterion) {
    let (pins, groups, player) = world(5000);
    let mut ui = Ui::new(Settings::default(), false);
    ui.opened(Some(&player));
    c.bench_function("rebuild_map_5000_markers", |b| {
        b.iter(|| {
            ui.rebuild_map(RECT, [100.0, 100.0], Some(&player), None, black_box(&pins), &groups);
            black_box(ui.map_ops().len())
        })
    });
    c.bench_function("rebuild_minimap_5000_markers", |b| {
        b.iter(|| {
            ui.rebuild_minimap(RECT, &player, None, black_box(&pins), &groups);
            black_box(ui.minimap_ops().len())
        })
    });
}

criterion_group!(group, benches);
criterion_main!(group);
