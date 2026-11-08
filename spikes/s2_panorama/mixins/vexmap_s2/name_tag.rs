fn vexmap_name_tag(visible: &mut bool) {
    if horizon_client_render::gfx::vexmap_s2::capturing() {
        *visible = false;
    }
}
