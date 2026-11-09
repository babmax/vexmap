//! Turns Horizon's region files into the schematic map's tile pyramid (ADR-004, ADR-021). Off the game thread and off the
//! game process: the sidecar runs this on a thread of its own, so a slow disk or a damaged file never reaches a tick.
//!
//! A pixel is one byte: the material in the low four bits and a shade in the high four. Colours are chosen on the client,
//! so a change of theme never touches a tile.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod read;
pub mod store;

pub use vexmap_proto::{Column, Material, classify};
use vexmap_proto::{TILE_AREA, TILE_SIZE};

/// A level-0 tile's pixel count on a region side: a region is 512 blocks, two tiles.
pub const REGION_BLOCKS: usize = 512;
const NEUTRAL_SHADE: u8 = 8;
const MAX_SHADE: u8 = 15;

/// The columns of one region, `REGION_BLOCKS` squared, west to east then north to south.
#[derive(Clone)]
pub struct RegionColumns {
    pub columns: Vec<Column>,
}

impl Default for RegionColumns {
    fn default() -> Self {
        RegionColumns { columns: vec![Column::default(); REGION_BLOCKS * REGION_BLOCKS] }
    }
}

pub fn pixel(material: Material, shade: u8) -> u8 {
    material as u8 | (shade.min(MAX_SHADE) << 4)
}

pub fn material_of(pixel: u8) -> u8 {
    pixel & 0x0F
}

pub fn shade_of(pixel: u8) -> u8 {
    pixel >> 4
}

/// Water gets darker with depth, in steps coarse enough to leave long runs for the codec.
fn water_shade(depth: u8) -> u8 {
    MAX_SHADE.saturating_sub(depth.min(30) / 3)
}

/// How far a difference in height moves the shade, in units of 1/2 step. Terrain from a north-west light, like a relief
/// map. Quantised, because a shade that flickers between neighbours costs the codec most of its runs.
fn slope_shade(rise: i32) -> u8 {
    (i32::from(NEUTRAL_SHADE) + (rise.clamp(-8, 7) / 2) * 2).clamp(0, i32::from(MAX_SHADE)) as u8
}

/// The two tiles of the region's level 0 row by row: `[north-west, north-east, south-west, south-east]`.
/// At the edge of a region the slope comes from the inside only, so neighbouring regions meet without a neutral seam.
pub fn rasterize(region: &RegionColumns) -> [Box<[u8; TILE_AREA]>; 4] {
    let mut tiles: [Box<[u8; TILE_AREA]>; 4] = std::array::from_fn(|_| Box::new([0; TILE_AREA]));
    let height = |x: usize, z: usize| i32::from(region.columns[z * REGION_BLOCKS + x].height);
    for z in 0..REGION_BLOCKS {
        for x in 0..REGION_BLOCKS {
            let column = region.columns[z * REGION_BLOCKS + x];
            let Some(material) = column.material else { continue };
            let shade = if material == Material::Water {
                water_shade(column.depth)
            } else if material == Material::Forest {
                // A canopy is bumpy at every block. Its relief says nothing and would cost most of the tile.
                NEUTRAL_SHADE
            } else {
                let (west, north) = (x.max(1) - 1, z.max(1) - 1);
                let (east, south) = (west + 1, north + 1);
                let rise = height(east, z) - height(west, z) + height(x, south) - height(x, north);
                slope_shade(rise)
            };
            let tile = usize::from(x >= TILE_SIZE) + 2 * usize::from(z >= TILE_SIZE);
            tiles[tile][(z % TILE_SIZE) * TILE_SIZE + x % TILE_SIZE] = pixel(material, shade);
        }
    }
    tiles
}

/// Half-size picture of four children, `[north-west, north-east, south-west, south-east]`. A missing child is unknown land.
/// Each output pixel keeps the most common known material of its 2x2 and the average shade of that material.
pub fn downsample(children: [Option<&[u8; TILE_AREA]>; 4]) -> Box<[u8; TILE_AREA]> {
    let mut out = Box::new([0; TILE_AREA]);
    let half = TILE_SIZE / 2;
    for (index, child) in children.iter().enumerate() {
        let Some(child) = child else { continue };
        let (left, top) = ((index % 2) * half, (index / 2) * half);
        for z in 0..half {
            for x in 0..half {
                let (a, b) = (z * 2 * TILE_SIZE + x * 2, (z * 2 + 1) * TILE_SIZE + x * 2);
                out[(top + z) * TILE_SIZE + left + x] = merge([child[a], child[a + 1], child[b], child[b + 1]]);
            }
        }
    }
    out
}

fn merge(pixels: [u8; 4]) -> u8 {
    let mut best = (0_u8, 0_u32, 0_u32);
    for candidate in pixels {
        let material = material_of(candidate);
        if material == 0 {
            continue;
        }
        let (mut count, mut shades) = (0, 0);
        for other in pixels {
            if material_of(other) == material {
                count += 1;
                shades += u32::from(shade_of(other));
            }
        }
        if count > best.1 {
            best = (material, count, shades);
        }
    }
    (best.2 + best.1 / 2).checked_div(best.1).map_or(0, |shade| best.0 | (shade as u8) << 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_classified_by_what_they_look_like_from_above() {
        assert_eq!(classify("minecraft:air"), None);
        assert_eq!(classify("minecraft:short_grass"), None);
        assert_eq!(classify("minecraft:oak_leaves"), Some(Material::Forest));
        assert_eq!(classify("minecraft:water"), Some(Material::Water));
        assert_eq!(classify("minecraft:stone_bricks"), Some(Material::Built));
        assert_eq!(classify("minecraft:glow_lichen"), None);
        assert_eq!(classify("minecraft:oak_sapling"), None);
    }

    fn flat(material: Material, height: i16) -> RegionColumns {
        let mut region = RegionColumns::default();
        region.columns.fill(Column { material: Some(material), height, depth: 0 });
        region
    }

    #[test]
    fn flat_land_is_neutral_and_splits_into_four_tiles() {
        let tiles = rasterize(&flat(Material::Grass, 64));
        for tile in &tiles {
            assert!(tile.iter().all(|pixel| material_of(*pixel) == 2 && shade_of(*pixel) == NEUTRAL_SHADE));
        }
    }

    #[test]
    fn an_unknown_column_stays_empty() {
        let tiles = rasterize(&RegionColumns::default());
        assert!(tiles[0].iter().all(|pixel| *pixel == 0));
    }

    #[test]
    fn a_slope_up_towards_the_south_east_faces_the_light_and_is_brighter_than_its_opposite() {
        let mut up = RegionColumns::default();
        let mut down = RegionColumns::default();
        for z in 0..REGION_BLOCKS {
            for x in 0..REGION_BLOCKS {
                let height = (x + z) as i16;
                up.columns[z * REGION_BLOCKS + x] = Column { material: Some(Material::Stone), height, depth: 0 };
                down.columns[z * REGION_BLOCKS + x] =
                    Column { material: Some(Material::Stone), height: -height, depth: 0 };
            }
        }
        assert!(shade_of(rasterize(&up)[0][300]) > NEUTRAL_SHADE);
        assert!(shade_of(rasterize(&down)[0][300]) < NEUTRAL_SHADE);
    }

    #[test]
    fn the_edge_of_a_region_is_shaded_like_its_inside() {
        let mut region = RegionColumns::default();
        for z in 0..REGION_BLOCKS {
            for x in 0..REGION_BLOCKS {
                region.columns[z * REGION_BLOCKS + x] =
                    Column { material: Some(Material::Stone), height: (x + z) as i16, depth: 0 };
            }
        }
        let tiles = rasterize(&region);
        assert_eq!(shade_of(tiles[0][0]), shade_of(tiles[0][TILE_SIZE + 1]));
    }

    #[test]
    fn deep_water_is_darker_than_shallow_water() {
        let mut region = flat(Material::Water, 62);
        region.columns[0].depth = 1;
        region.columns[1].depth = 25;
        let tiles = rasterize(&region);
        assert!(shade_of(tiles[0][1]) < shade_of(tiles[0][0]));
    }

    #[test]
    fn downsampling_keeps_the_common_material_and_ignores_the_unknown() {
        assert_eq!(
            material_of(merge([pixel(Material::Grass, 8), pixel(Material::Grass, 4), pixel(Material::Sand, 2), 0])),
            2
        );
        assert_eq!(shade_of(merge([pixel(Material::Grass, 8), pixel(Material::Grass, 4), 0, 0])), 6);
        assert_eq!(merge([0; 4]), 0);
    }

    #[test]
    fn four_children_make_one_parent_in_the_right_quadrants() {
        let mut grass = Box::new([0; TILE_AREA]);
        grass.fill(pixel(Material::Grass, 8));
        let mut sand = Box::new([0; TILE_AREA]);
        sand.fill(pixel(Material::Sand, 8));
        let parent = downsample([Some(&grass), Some(&sand), None, Some(&grass)]);
        assert_eq!(material_of(parent[0]), 2);
        assert_eq!(material_of(parent[TILE_SIZE - 1]), 4);
        assert_eq!(parent[(TILE_SIZE - 1) * TILE_SIZE], 0);
        assert_eq!(material_of(parent[TILE_AREA - 1]), 2);
    }
}
