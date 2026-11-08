fn vexmap_key_pressed(game: &crate::game::Game<'_>, key: &str, repeat: bool, canceled: &mut bool) {
    crate::game::vexmap_screen::key_pressed(game, key, repeat, canceled);
}
