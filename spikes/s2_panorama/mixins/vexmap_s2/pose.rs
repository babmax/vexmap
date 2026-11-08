fn vexmap_pose(position: &mut [f64; 3], yaw: &mut f32, pitch: &mut f32) {
    if let Some((at, face_yaw, face_pitch)) = horizon_client_render::gfx::vexmap_s2::pose() {
        *position = at;
        *yaw = face_yaw;
        *pitch = face_pitch;
    }
}
