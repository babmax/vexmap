fn vexmap_fov(degrees: &mut f32) {
    if horizon_client_render::gfx::vexmap_s2::capturing() {
        *degrees = 90.0;
    }
}
