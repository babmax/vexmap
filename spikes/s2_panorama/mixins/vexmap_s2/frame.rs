fn vexmap_frame(game: &crate::game::Game<'_>) {
    let eye = game.native_player().map(|p| [p.x, p.y + 1.62, p.z]);
    horizon_client_render::gfx::vexmap_s2::frame(eye);
}
