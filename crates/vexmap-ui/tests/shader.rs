//! The map's shader is compiled only when the game first opens the map, so a mistake would otherwise reach players.

#[test]
fn the_map_shader_is_valid_wgsl() {
    let source = include_str!("../../../mods/vexmap/mixins/vexmap/render.rs");
    let start = source.find("r#\"").expect("the shader string") + 3;
    let length = source[start..].find("\"#").expect("the end of the shader string");
    let module = naga::front::wgsl::parse_str(&source[start..start + length]).expect("WGSL that parses");
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
        .validate(&module)
        .expect("WGSL that validates");
}

/// The buffer in render.rs is written field by field, so it must match the shader's struct exactly.
#[test]
fn the_uniform_buffer_has_the_size_of_the_shader_struct() {
    let source = include_str!("../../../mods/vexmap/mixins/vexmap/render.rs");
    let start = source.find("r#\"").expect("the shader string") + 3;
    let length = source[start..].find("\"#").expect("the end of the shader string");
    let module = naga::front::wgsl::parse_str(&source[start..start + length]).expect("WGSL that parses");
    let params = module
        .types
        .iter()
        .find_map(|(_, ty)| match &ty.inner {
            naga::TypeInner::Struct { span, .. } if ty.name.as_deref() == Some("Params") => Some(*span),
            _ => None,
        })
        .expect("the Params struct");
    assert!(source.contains("const UNIFORM_BYTES: usize = 8 * 16 + 16 * 16;"));
    assert_eq!(params, 8 * 16 + 16 * 16);
}
