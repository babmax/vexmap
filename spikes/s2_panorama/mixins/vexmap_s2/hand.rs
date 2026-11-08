fn vexmap_hand(canceled: &mut bool) {
    if horizon_client_render::gfx::vexmap_s2::capturing() {
        *canceled = true;
    }
}
