//! Reads a real region file and reports what the tiler made of it:
//! `cargo run --release -p vexmap-tiler --example probe -- <r.x.z.mca> [dimension]`
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use vexmap_proto::encode_tile;
use vexmap_tiler::read::{Survey, read_region};
use vexmap_tiler::{material_of, rasterize};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: probe <region file> [dimension]")?;
    let dimension = std::env::args().nth(2).unwrap_or_else(|| "minecraft:overworld".to_owned());
    let started = Instant::now();
    let (columns, report) = read_region(Path::new(&path), Survey::of_dimension(&dimension))?;
    let read = started.elapsed();
    let started = Instant::now();
    let tiles = rasterize(&columns);
    let drawn = started.elapsed();
    let mut materials: BTreeMap<u8, usize> = BTreeMap::new();
    let mut sizes = Vec::new();
    for tile in &tiles {
        for pixel in tile.iter() {
            *materials.entry(material_of(*pixel)).or_default() += 1;
        }
        let mut encoded = Vec::new();
        encode_tile(&tile[..], &mut encoded);
        sizes.push(encoded.len());
    }
    println!("{report:?} read {read:?} rasterize {drawn:?}");
    println!("encoded tile bytes {sizes:?}");
    println!("materials {materials:?}");
    Ok(())
}
