fn vexmap_register_commands(commands: &mut Vec<horizon_client::mod_api::NativeCommand>) {
    crate::game::vexmap::register_commands(commands);
}
