fn vexmap_frame(game: &crate::game::Game<'_>) {
    horizon_client_render::gfx::vexmap_s1::set_visible(game.native_mod_screen() == Some("vexmap_s1"));
}
