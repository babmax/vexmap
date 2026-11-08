//! Reads one region file into columns, through Horizon's own world crate (ADR-004).
//!
//! The server may be writing the file while we read it. A chunk that fails to decode is left unknown and reported, and the
//! caller tries the region again at its next scan.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use horizon_world::chunk::PalettedContainer;
use horizon_world::nbt::{Limits, Tag};
use horizon_world::region::RegionFile;

use crate::{Column, Material, REGION_BLOCKS, RegionColumns, classify};

const SECTION_BLOCKS: usize = 4096;
const BLOCK_STATE_MIN_BITS: u8 = 4;
const FULL_STATUS: &str = "minecraft:full";

/// How to look at a dimension from above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Survey {
    /// Start at or below this height.
    pub ceiling: i32,
    /// Roofed: walk down out of the solid roof first, so the map shows the caves and not the top of the roof.
    pub roofed: bool,
}

impl Survey {
    pub fn of_dimension(dimension: &str) -> Survey {
        if dimension == "minecraft:the_nether" {
            Survey { ceiling: 126, roofed: true }
        } else {
            Survey { ceiling: i32::MAX, roofed: false }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReadReport {
    pub chunks: usize,
    /// Chunks that could not be decoded, probably because the server is writing them.
    pub damaged: usize,
}

struct Section {
    y: i32,
    blocks: PalettedContainer,
}

/// Block names are UTF-16 in NBT and repeat endlessly, so each is classified once per region.
type Names = HashMap<Vec<u16>, Option<Material>>;

fn sections_of(root: &Tag) -> io::Result<Vec<Section>> {
    let Some(Tag::List(list)) = root.get("sections") else { return Ok(Vec::new()) };
    let mut sections = Vec::with_capacity(list.len());
    for section in list {
        let y = match section.get("Y") {
            Some(Tag::Byte(y)) => i32::from(*y),
            Some(Tag::Int(y)) => *y,
            _ => continue,
        };
        let Some(states) = section.get("block_states") else { continue };
        sections
            .push(Section { y, blocks: PalettedContainer::from_nbt(states, SECTION_BLOCKS, BLOCK_STATE_MIN_BITS)? });
    }
    // Top first, which is the order the columns are walked.
    sections.sort_by_key(|section| std::cmp::Reverse(section.y));
    Ok(sections)
}

fn lookup(section: &Section, index: usize, names: &mut Names) -> io::Result<Option<Material>> {
    let state = section.blocks.get(index)?;
    let Some(Tag::String(name)) = state.get("Name").or_else(|| state.get("id")) else { return Ok(None) };
    if let Some(known) = names.get(name.0.as_slice()) {
        return Ok(*known);
    }
    let material = classify(&name.to_string_lossy());
    names.insert(name.0.clone(), material);
    Ok(material)
}

fn column_of(sections: &[Section], x: usize, z: usize, survey: Survey, names: &mut Names) -> io::Result<Column> {
    let mut in_roof = survey.roofed;
    let mut water_top: Option<i32> = None;
    for section in sections {
        let base = section.y * 16;
        if base > survey.ceiling {
            continue;
        }
        let highest = survey.ceiling.saturating_sub(base).min(15);
        for local in (0..=highest).rev() {
            let index = (local as usize) << 8 | z << 4 | x;
            let material = lookup(section, index, names)?;
            if in_roof {
                in_roof = material.is_some();
                continue;
            }
            let height = base + local;
            match (water_top, material) {
                (None, None) => {}
                (None, Some(Material::Water)) => water_top = Some(height),
                (None, Some(found)) => return Ok(Column { material: Some(found), height: height as i16, depth: 0 }),
                (Some(_), Some(Material::Water) | None) => {}
                (Some(surface), Some(_)) => {
                    let depth = (surface - height).clamp(1, i32::from(u8::MAX)) as u8;
                    return Ok(Column { material: Some(Material::Water), height: surface as i16, depth });
                }
            }
        }
    }
    Ok(match water_top {
        Some(surface) => Column { material: Some(Material::Water), height: surface as i16, depth: u8::MAX },
        None => Column::default(),
    })
}

fn draw_chunk(
    root: &Tag,
    left: usize,
    top: usize,
    survey: Survey,
    columns: &mut RegionColumns,
    names: &mut Names,
) -> io::Result<()> {
    match root.get("Status") {
        Some(Tag::String(status)) if status.to_string_lossy() == FULL_STATUS => {}
        _ => return Ok(()),
    }
    let sections = sections_of(root)?;
    for z in 0..16 {
        for x in 0..16 {
            columns.columns[(top + z) * REGION_BLOCKS + left + x] = column_of(&sections, x, z, survey, names)?;
        }
    }
    Ok(())
}

pub fn read_region(path: &Path, survey: Survey) -> io::Result<(RegionColumns, ReadReport)> {
    let mut region = RegionFile::open(path)?;
    let mut columns = RegionColumns::default();
    let mut report = ReadReport::default();
    let mut names = Names::new();
    let limits = Limits::default();
    let positions: Vec<_> = region.chunks().map(|(position, _)| position).collect();
    for position in positions {
        report.chunks += 1;
        let (left, top) = (position.x.rem_euclid(32) as usize * 16, position.z.rem_euclid(32) as usize * 16);
        let drawn = region.read(position).and_then(|record| match record {
            Some(record) => draw_chunk(&record.nbt(limits)?.tag, left, top, survey, &mut columns, &mut names),
            None => Ok(()),
        });
        if drawn.is_err() {
            report.damaged += 1;
        }
    }
    Ok((columns, report))
}

#[cfg(test)]
mod tests {
    use horizon_world::nbt::JavaString;

    use super::*;

    fn block(name: &str) -> Tag {
        Tag::compound([("Name", Tag::String(JavaString::from(name)))])
    }

    fn section(y: i32, name: &str) -> Section {
        Section { y, blocks: PalettedContainer::uniform(block(name), SECTION_BLOCKS, BLOCK_STATE_MIN_BITS).unwrap() }
    }

    fn column(sections: &[Section], survey: Survey) -> Column {
        column_of(sections, 3, 5, survey, &mut Names::new()).unwrap()
    }

    const OPEN: Survey = Survey { ceiling: i32::MAX, roofed: false };

    #[test]
    fn a_column_below_zero_is_found() {
        // An open dimension has no ceiling, and sections below y 0 have a negative base: this overflowed once.
        let column = column(&[section(0, "minecraft:air"), section(-1, "minecraft:deepslate")], OPEN);
        assert_eq!((column.material, column.height), (Some(Material::Stone), -1));
    }

    #[test]
    fn plants_are_looked_through() {
        let column = column(&[section(4, "minecraft:short_grass"), section(3, "minecraft:grass_block")], OPEN);
        assert_eq!((column.material, column.height), (Some(Material::Grass), 63));
    }

    #[test]
    fn water_records_its_surface_and_depth() {
        let column = column(&[section(3, "minecraft:water"), section(2, "minecraft:sand")], OPEN);
        assert_eq!((column.material, column.height, column.depth), (Some(Material::Water), 63, 16));
    }

    #[test]
    fn a_roofed_dimension_shows_the_floor_under_the_roof() {
        let nether = Survey::of_dimension("minecraft:the_nether");
        let sections =
            [section(7, "minecraft:netherrack"), section(6, "minecraft:air"), section(5, "minecraft:warped_nylium")];
        let column = column(&sections, nether);
        assert_eq!((column.material, column.height), (Some(Material::NetherGrowth), 95));
    }

    #[test]
    fn an_empty_column_is_unknown() {
        assert_eq!(column(&[section(0, "minecraft:air")], OPEN), Column::default());
    }
}
