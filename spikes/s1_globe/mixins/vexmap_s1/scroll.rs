fn vexmap_scroll(game: &crate::game::Game<'_>, scroll: [f64; 2]) {
    if game.native_mod_screen() == Some("vexmap_s1") {
        horizon_client_render::gfx::vexmap_s1::scroll(scroll[1] as f32);
    }
}
