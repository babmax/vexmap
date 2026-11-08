fn vexmap_from_server(game: &mut crate::game::Game<'_>, channel: &str, data: &[u8]) {
    crate::game::vexmap_s3::from_server(game, channel, data);
}
