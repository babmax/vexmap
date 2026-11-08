fn vexmap_mouse_dragged(game: &crate::game::Game<'_>, delta: [f32; 2], canceled: &mut bool) {
    crate::game::vexmap_screen::mouse_dragged(game, delta, canceled);
}
