fn vexmap_drag(game: &crate::game::Game<'_>, delta: [f32; 2]) {
    if game.native_mod_screen() == Some("vexmap_s1") {
        horizon_client_render::gfx::vexmap_s1::drag(delta);
    }
}
