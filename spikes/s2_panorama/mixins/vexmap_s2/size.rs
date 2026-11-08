fn vexmap_size(renderer: &crate::gfx::Renderer) -> std::ops::ControlFlow<Option<[u32; 2]>> {
    match crate::gfx::vexmap_s2::capture_size([renderer.config.width, renderer.config.height]) {
        Some(size) => std::ops::ControlFlow::Break(Some(size)),
        None => std::ops::ControlFlow::Continue(()),
    }
}
