fn vexmap_mouse_scrolled(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    scroll: [f64; 2],
    canceled: &mut bool,
) {
    crate::game::vexmap_screen::mouse_scrolled(game, screen, scroll, canceled);
}
