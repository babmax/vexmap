use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use vexmap_proto::{TILE_AREA, decode_tile, encode_tile};
use vexmap_tiler::{Column, Material, REGION_BLOCKS, RegionColumns, downsample, rasterize};

/// Rolling land with a shoreline: the shapes a real region has, without a real file in the repository.
fn terrain() -> RegionColumns {
    let mut region = RegionColumns::default();
    for z in 0..REGION_BLOCKS {
        for x in 0..REGION_BLOCKS {
            let (fx, fz) = (x as f64, z as f64);
            let height = 64.0 + 9.0 * (fx / 37.0).sin() + 7.0 * (fz / 23.0).cos() + 3.0 * ((fx + fz) / 11.0).sin();
            let column = if height < 60.0 {
                Column { material: Some(Material::Water), height: 62, depth: (62.0 - height) as u8 }
            } else if height > 74.0 {
                Column { material: Some(Material::Forest), height: height as i16, depth: 0 }
            } else {
                Column { material: Some(Material::Grass), height: height as i16, depth: 0 }
            };
            region.columns[z * REGION_BLOCKS + x] = column;
        }
    }
    region
}

fn bench(c: &mut Criterion) {
    let region = terrain();
    let tiles = rasterize(&region);
    let mut encoded = Vec::new();
    encode_tile(&tiles[0][..], &mut encoded);
    eprintln!("encoded tile: {} bytes of {TILE_AREA}", encoded.len());
    c.bench_function("rasterize_region", |b| b.iter(|| rasterize(black_box(&region))));
    c.bench_function("downsample_tile", |b| {
        b.iter(|| downsample(black_box([Some(&*tiles[0]), Some(&*tiles[1]), Some(&*tiles[2]), Some(&*tiles[3])])))
    });
    c.bench_function("encode_tile", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(TILE_AREA);
            encode_tile(black_box(&tiles[0][..]), &mut out);
            out
        })
    });
    let mut out = Box::new([0; TILE_AREA]);
    c.bench_function("decode_tile", |b| b.iter(|| decode_tile(black_box(&encoded), &mut out)));
}

criterion_group!(benches, bench);
criterion_main!(benches);
