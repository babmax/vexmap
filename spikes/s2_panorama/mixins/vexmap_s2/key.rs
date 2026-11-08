fn vexmap_key(key: &str, pressed: bool) {
    if pressed && key == "key.keyboard.n" {
        horizon_client_render::gfx::vexmap_s2::toggle_auto();
    }
}
