fn vexmap_mouse_pressed(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    crate::game::vexmap_screen::mouse_pressed(game, screen, button, canceled);
}
