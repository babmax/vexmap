fn vexmap_client_packet(game: &mut crate::game::Game<'_>, packet: &horizon_client::mod_api::ClientboundPlayPacket) {
    crate::game::vexmap_s3::client_packet(game, packet);
}
