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

pub use vexmap_proto::Survey;
use vexmap_proto::walk_column;

use crate::{Column, Material, REGION_BLOCKS, RegionColumns, classify};

const SECTION_BLOCKS: usize = 4096;
const BLOCK_STATE_MIN_BITS: u8 = 4;
const FULL_STATUS: &str = "minecraft:full";

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
    // A block with no properties is stored as just its name. It is most of the world: stone, dirt, still water.
    let name = match state {
        Tag::String(name) => name,
        _ => match state.get("Name").or_else(|| state.get("id")) {
            Some(Tag::String(name)) => name,
            _ => return Ok(None),
        },
    };
    if let Some(known) = names.get(name.0.as_slice()) {
        return Ok(*known);
    }
    let material = classify(&name.to_string_lossy());
    names.insert(name.0.clone(), material);
    Ok(material)
}

fn column_of(sections: &[Section], x: usize, z: usize, survey: Survey, names: &mut Names) -> io::Result<Column> {
    let (Some(first), Some(last)) = (sections.first(), sections.last()) else { return Ok(Column::default()) };
    let (top, bottom) = (first.y * 16 + 15, last.y * 16);
    let mut failure = None;
    // Heights only go down, so the section in hand only moves forward.
    let mut at = 0;
    let column = walk_column(survey, top, bottom, |height| {
        let wanted = height.div_euclid(16);
        while sections.get(at).is_some_and(|section| section.y > wanted) {
            at += 1;
        }
        let section = sections.get(at).filter(|section| section.y == wanted)?;
        let index = (height.rem_euclid(16) as usize) << 8 | z << 4 | x;
        lookup(section, index, names).unwrap_or_else(|error| {
            failure = Some(error);
            None
        })
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(column),
    }
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

    /// The three ways a palette entry is written, oldest first. Real files mix the last two within one section.
    #[test]
    fn every_form_of_a_palette_entry_is_understood() {
        let name = |text: &str| Tag::String(JavaString::from(text));
        let forms =
            [block("minecraft:stone"), Tag::compound([("id", name("minecraft:stone"))]), name("minecraft:stone")];
        for form in forms {
            let blocks = PalettedContainer::uniform(form.clone(), SECTION_BLOCKS, BLOCK_STATE_MIN_BITS).unwrap();
            let column = column(&[Section { y: 4, blocks }], OPEN);
            assert_eq!(column.material, Some(Material::Stone), "{form:?}");
        }
    }

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
