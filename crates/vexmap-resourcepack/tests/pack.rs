use std::io::{Cursor, Read};
use vexmap_resourcepack::{GLYPHS, PACK_FORMAT, build, files, sha1_hex};

#[test]
fn the_same_input_gives_the_same_bytes() {
    assert_eq!(sha1_hex(&build().unwrap()), sha1_hex(&build().unwrap()));
}

#[test]
fn the_sha1_is_40_lowercase_hex_digits_as_the_client_expects() {
    let hash = sha1_hex(b"abc");
    assert_eq!(hash, "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn the_pack_stays_small() {
    // Every player downloads it through the pack host on their first join.
    assert!(build().unwrap().len() < 64 * 1024);
}

#[test]
fn the_zip_opens_and_holds_every_file() {
    let bytes = build().unwrap();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    for (path, expected) in files().unwrap() {
        let mut found = Vec::new();
        archive.by_name(path).unwrap().read_to_end(&mut found).unwrap();
        assert_eq!(found, expected, "{path}");
    }
}

#[test]
fn nothing_outside_our_namespace_but_the_merged_font_and_sprite_folder() {
    for (path, _) in files().unwrap() {
        let ours = path == "pack.mcmeta"
            || path.starts_with("assets/vexmap/")
            || path.starts_with("assets/minecraft/textures/gui/sprites/vexmap/")
            || path == "assets/minecraft/font/default.json";
        assert!(ours, "{path}");
    }
}

fn file(path: &str) -> Vec<u8> {
    files().unwrap().into_iter().find(|(name, _)| *name == path).unwrap().1
}

#[test]
fn metadata_declares_horizons_format() {
    let meta = String::from_utf8(file("pack.mcmeta")).unwrap();
    assert!(meta.contains(&format!("\"min_format\":{PACK_FORMAT}")), "{meta}");
}

#[test]
fn the_font_names_every_glyph_and_stays_in_the_private_use_area() {
    let font = String::from_utf8(file("assets/minecraft/font/default.json")).unwrap();
    for glyph in GLYPHS {
        assert!((0xE000..=0xF8FF).contains(&u32::from(glyph)));
        assert!(font.contains(&format!("\\u{:04x}", u32::from(glyph))), "{font}");
    }
}

#[test]
fn images_are_png_of_the_declared_size_and_not_blank() {
    for path in ["assets/minecraft/textures/gui/sprites/vexmap/map_button.png", "assets/vexmap/textures/font/icons.png"]
    {
        let bytes = file(path);
        let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
        let mut image = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut image).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert!(image.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 255), "{path}");
        assert!(image.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 0), "{path}");
    }
}
