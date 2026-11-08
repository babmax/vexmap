fn vexmap_from_client(channel: &str, data: &[u8]) {
    crate::game::vexmap_s3::from_client(channel, data);
}
