fn vexmap_key(game: &mut crate::game::Game<'_>, key: &str, pressed: bool) {
    if pressed && key == "key.keyboard.m" && game.native_playing() && game.native_mod_screen().is_none() {
        game.native_mod_screen_open("vexmap_s1");
    }
}
