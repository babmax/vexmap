//! The VexMap server resource pack (ADR-008, decision 133). Every image is computed here from simple shapes, so the repository
//! holds no binary assets and nothing of Mojang's or Google's.
//!
//! The same input gives the same bytes: files are sorted, timestamps fixed. Clients cache a pack by its SHA-1, so a pack
//! that does not change must keep its hash.

use sha1::{Digest, Sha1};
use std::io::{Cursor, Write};
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::{CompressionMethod, DateTime};

/// Horizon's `RESOURCE_PACK_FORMAT` major at the pinned commit. A pack outside the client's range is refused or warned about.
pub const PACK_FORMAT: u32 = 97;
/// A fixed id: a new version of the pack replaces the old one on the client instead of piling up beside it.
pub const PACK_ID: u128 = 0x5645_584d_4150_5041_434b_0000_0000_0001;
/// `native_sprite` names of the GUI sprites, as the client mod asks for them.
pub const SPRITE_MAP_BUTTON: &str = "gui/sprites/vexmap/map_button";
/// Private-use characters with an icon in the default font: for chest-menu titles, which cannot use another font (ADR-008).
pub const GLYPHS: [char; 4] = ['\u{e000}', '\u{e001}', '\u{e002}', '\u{e003}'];

const GLYPH_SIZE: u32 = 16;
const SPRITE_SIZE: u32 = 32;
const SUPERSAMPLE: u32 = 4;

#[derive(Debug)]
pub enum PackError {
    Png(png::EncodingError),
    Zip(zip::result::ZipError),
    Io(std::io::Error),
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::Png(error) => write!(f, "png: {error}"),
            PackError::Zip(error) => write!(f, "zip: {error}"),
            PackError::Io(error) => write!(f, "io: {error}"),
        }
    }
}

impl std::error::Error for PackError {}

impl From<png::EncodingError> for PackError {
    fn from(error: png::EncodingError) -> Self {
        PackError::Png(error)
    }
}

impl From<zip::result::ZipError> for PackError {
    fn from(error: zip::result::ZipError) -> Self {
        PackError::Zip(error)
    }
}

impl From<std::io::Error> for PackError {
    fn from(error: std::io::Error) -> Self {
        PackError::Io(error)
    }
}

/// A shape: alpha and colour at a point of the unit square, `None` outside it.
type Shape = fn(f32, f32) -> Option<[u8; 3]>;

const PAPER: [u8; 3] = [0xF1, 0xEE, 0xE2];
const FOLD: [u8; 3] = [0xC9, 0xC4, 0xB0];
const WATER: [u8; 3] = [0x4F, 0x9D, 0xFF];
const PIN: [u8; 3] = [0xE5, 0x4B, 0x3C];
const MINT: [u8; 3] = [0x35, 0xD0, 0xA5];
const AMBER: [u8; 3] = [0xFF, 0xB5, 0x47];

fn circle(x: f32, y: f32, cx: f32, cy: f32, radius: f32) -> bool {
    (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
}

/// Which side of the line from `a` to `b` a point is on.
fn side(x: f32, y: f32, a: [f32; 2], b: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0])
}

fn triangle(x: f32, y: f32, a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> bool {
    let (d1, d2, d3) = (side(x, y, a, b), side(x, y, b, c), side(x, y, c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

/// A folded paper map with a pin on it.
fn map_button(x: f32, y: f32) -> Option<[u8; 3]> {
    if circle(x, y, 0.64, 0.34, 0.15) {
        return Some(if circle(x, y, 0.64, 0.34, 0.06) { PAPER } else { PIN });
    }
    if triangle(x, y, [0.53, 0.42], [0.75, 0.42], [0.64, 0.66]) {
        return Some(PIN);
    }
    if !(0.08..=0.92).contains(&x) {
        return None;
    }
    // The three panels of the fold lean in turn, so the top and bottom edges zigzag.
    let panel = (((x - 0.08) / 0.28) as usize).min(2);
    let slope = if panel == 1 { -1.0 } else { 1.0 };
    let along = x - 0.08 - panel as f32 * 0.28;
    let offset = if panel == 1 { 0.08 } else { 0.0 } + slope * along * 0.28;
    if y < 0.16 + offset || y > 0.84 + offset {
        return None;
    }
    if (x - 0.36).abs() < 0.012 || (x - 0.64).abs() < 0.012 {
        return Some(FOLD);
    }
    Some(if (y - 0.62 - offset).abs() < 0.07 { WATER } else { PAPER })
}

fn glyph_pin(x: f32, y: f32) -> Option<[u8; 3]> {
    if circle(x, y, 0.5, 0.36, 0.3) {
        return Some(if circle(x, y, 0.5, 0.36, 0.12) { PAPER } else { PIN });
    }
    triangle(x, y, [0.25, 0.5], [0.75, 0.5], [0.5, 0.96]).then_some(PIN)
}

fn glyph_flag(x: f32, y: f32) -> Option<[u8; 3]> {
    if (0.18..0.28).contains(&x) && (0.06..0.96).contains(&y) {
        return Some(FOLD);
    }
    triangle(x, y, [0.28, 0.1], [0.28, 0.6], [0.92, 0.35]).then_some(MINT)
}

fn glyph_star(x: f32, y: f32) -> Option<[u8; 3]> {
    let (dx, dy) = (x - 0.5, y - 0.52);
    let angle = dy.atan2(dx) + std::f32::consts::FRAC_PI_2;
    let wedge = std::f32::consts::TAU / 5.0;
    let fold = (angle.rem_euclid(wedge) - wedge / 2.0).abs() / (wedge / 2.0);
    let radius = 0.2 + 0.28 * fold.powi(2);
    (dx * dx + dy * dy <= radius * radius).then_some(AMBER)
}

fn glyph_globe(x: f32, y: f32) -> Option<[u8; 3]> {
    if !circle(x, y, 0.5, 0.5, 0.44) {
        return None;
    }
    let land = circle(x, y, 0.36, 0.38, 0.17) || circle(x, y, 0.66, 0.66, 0.15);
    Some(if land { MINT } else { WATER })
}

/// `shape` over a `size`-square, supersampled so diagonal edges are smooth.
fn render(shape: Shape, size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let steps = SUPERSAMPLE * SUPERSAMPLE;
    for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (px, py) = (index as u32 % size, index as u32 / size);
        let (mut sum, mut hits) = ([0u32; 3], 0u32);
        for sub in 0..steps {
            let (sx, sy) = (sub % SUPERSAMPLE, sub / SUPERSAMPLE);
            let x = (px as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
            let y = (py as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
            if let Some(color) = shape(x, y) {
                hits += 1;
                for (total, channel) in sum.iter_mut().zip(color) {
                    *total += u32::from(channel);
                }
            }
        }
        for (out, total) in pixel.iter_mut().zip(sum) {
            *out = total.checked_div(hits).unwrap_or(0) as u8;
        }
        pixel[3] = (hits * 255 / steps) as u8;
    }
    rgba
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, PackError> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(rgba)?;
    Ok(out)
}

/// One row of glyphs. Vanilla's `bitmap` provider cuts the sheet into equal cells.
fn glyph_sheet() -> Result<Vec<u8>, PackError> {
    let shapes: [Shape; 4] = [glyph_pin, glyph_flag, glyph_star, glyph_globe];
    let width = GLYPH_SIZE * shapes.len() as u32;
    let mut sheet = vec![0u8; (width * GLYPH_SIZE * 4) as usize];
    for (column, shape) in shapes.iter().enumerate() {
        let tile = render(*shape, GLYPH_SIZE);
        for (row, line) in tile.as_chunks::<{ (GLYPH_SIZE * 4) as usize }>().0.iter().enumerate() {
            let start = (row * (width * 4) as usize) + column * (GLYPH_SIZE * 4) as usize;
            sheet[start..start + line.len()].copy_from_slice(line);
        }
    }
    encode_png(width, GLYPH_SIZE, &sheet)
}

fn json_escape_char(c: char) -> String {
    format!("\\u{:04x}", u32::from(c))
}

fn font_json() -> String {
    let chars: String = GLYPHS.iter().map(|c| json_escape_char(*c)).collect();
    format!(
        "{{\"providers\":[{{\"type\":\"bitmap\",\"file\":\"vexmap:font/icons.png\",\"ascent\":7,\"height\":8,\"chars\":[\"{chars}\"]}}]}}\n"
    )
}

fn pack_meta() -> String {
    format!(
        "{{\"pack\":{{\"description\":\"VexMap icons\",\"min_format\":{PACK_FORMAT},\"max_format\":{PACK_FORMAT},\"pack_format\":{PACK_FORMAT}}}}}\n"
    )
}

/// The pack's files, sorted by path.
pub fn files() -> Result<Vec<(&'static str, Vec<u8>)>, PackError> {
    let sprite = render(map_button, SPRITE_SIZE);
    let mut files = vec![
        ("assets/minecraft/font/default.json", font_json().into_bytes()),
        ("assets/minecraft/textures/gui/sprites/vexmap/map_button.png", encode_png(SPRITE_SIZE, SPRITE_SIZE, &sprite)?),
        ("assets/vexmap/textures/font/icons.png", glyph_sheet()?),
        ("pack.mcmeta", pack_meta().into_bytes()),
    ];
    files.sort_by_key(|(path, _)| *path);
    Ok(files)
}

/// The finished zip. Stable for a given version of this crate and its `zip` and `png` dependencies.
pub fn build() -> Result<Vec<u8>, PackError> {
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o644);
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (path, bytes) in files()? {
        writer.start_file(path, options)?;
        writer.write_all(&bytes)?;
    }
    Ok(writer.finish()?.into_inner())
}

/// Lowercase hex, as the client compares it.
pub fn sha1_hex(bytes: &[u8]) -> String {
    Sha1::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
