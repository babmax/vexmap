// The map's ground on the GPU, through `hand_drawn`: after the world, before the HUD and our GUI overlay (ADR-018, S1).
// One quad and one fragment shader for all three modes. For each pixel the shader finds the world point, looks the tile up
// in a page table and colours the material from the palette. The logic that fills the page tables lives in ui.rs.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::native_render::HandDrawn;

const TILE_SIZE: u32 = 256;
/// `vexmap_ui::TILE_SLOTS`, `PAGE_SIZE`, `PAGE_BYTES`.
const TILE_SLOTS: u32 = 256;
const PAGE_SIZE: u32 = 64;
const PAGE_BYTES: usize = 2 * 64 * 64 * 4;
const TILE_AREA: usize = 256 * 256;
/// Tiles uploaded in one frame. The rest wait, so a burst of tiles cannot stall a frame.
const UPLOADS_PER_FRAME: usize = 16;
const UNIFORM_BYTES: usize = 8 * 16 + 16 * 16;

/// One frame of the map, in plain numbers. See `vexmap_ui::MapScene`.
pub struct Scene {
    pub rect: [f32; 4],
    pub mode: u32,
    pub center: [f32; 2],
    pub half_extent: [f32; 2],
    pub camera: [f32; 3],
    pub theta_max: f32,
    pub disc: [f32; 3],
    pub night: f32,
    pub fine: [i32; 3],
    pub coarse: [i32; 3],
    pub palette: [[f32; 4]; 16],
    pub turn: [f32; 4],
    pub opacity: f32,
    pub cut_out: bool,
    /// Only when the tables changed.
    pub pages: Option<Box<[u8; PAGE_BYTES]>>,
}

#[derive(Default)]
struct Shared {
    scene: Option<Scene>,
    uploads: Vec<(u16, Box<[u8; TILE_AREA]>)>,
    /// Tables waiting for the next frame. Kept apart from `scene`, which the next frame's scene replaces.
    pages: Option<Box<[u8; PAGE_BYTES]>>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { scene: None, uploads: Vec::new(), pages: None });
static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Counts the GPU resources built, so the game's tile cache can tell its tiles are gone.
static GPU_EPOCH: AtomicU64 = AtomicU64::new(0);

struct State {
    device: wgpu::Device,
    format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    group: wgpu::BindGroup,
    params: wgpu::Buffer,
    tiles: wgpu::Texture,
    pages: wgpu::Texture,
}

/// `None` hides the map.
pub fn set_scene(mut scene: Option<Scene>) {
    if let Ok(mut shared) = SHARED.lock() {
        if let Some(pages) = scene.as_mut().and_then(|scene| scene.pages.take()) {
            shared.pages = Some(pages);
        }
        shared.scene = scene;
    }
}

pub fn gpu_epoch() -> u64 {
    GPU_EPOCH.load(Ordering::Relaxed)
}

pub fn queue_tile(slot: u16, pixels: Box<[u8; TILE_AREA]>) {
    if let Ok(mut shared) = SHARED.lock() {
        shared.uploads.retain(|(queued, _)| *queued != slot);
        shared.uploads.push((slot, pixels));
    }
}

fn texture(
    device: &wgpu::Device,
    label: &str,
    size: u32,
    layers: u32,
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: layers },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    (texture, view)
}

fn unsigned_array(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Uint,
            view_dimension: wgpu::TextureViewDimension::D2Array,
            multisampled: false,
        },
        count: None,
    }
}

fn build(device: &wgpu::Device, format: wgpu::TextureFormat) -> State {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("vexmap map"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vexmap map"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            unsigned_array(1),
            unsigned_array(2),
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vexmap map"),
        bind_group_layouts: &[Some(&layout)],
        ..Default::default()
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("vexmap map"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::COLOR,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let params = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vexmap map"),
        size: UNIFORM_BYTES as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let (tiles, tiles_view) = texture(device, "vexmap tiles", TILE_SIZE, TILE_SLOTS, wgpu::TextureFormat::R8Uint);
    let (pages, pages_view) = texture(device, "vexmap pages", PAGE_SIZE, 2, wgpu::TextureFormat::Rgba8Uint);
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vexmap map"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&tiles_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&pages_view) },
        ],
    });
    GPU_EPOCH.fetch_add(1, Ordering::Relaxed);
    State { device: device.clone(), format, pipeline, group, params, tiles, pages }
}

fn write_layers(
    queue: &wgpu::Queue,
    target: &wgpu::Texture,
    layer: u32,
    layers: u32,
    size: u32,
    texel: u32,
    data: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: target,
            mip_level: 0,
            origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size * texel), rows_per_image: Some(size) },
        wgpu::Extent3d { width: size, height: size, depth_or_array_layers: layers },
    );
}

fn uniforms(scene: &Scene, size: [u32; 2]) -> [u8; UNIFORM_BYTES] {
    let pixels = [scene.rect[2] * size[0] as f32, scene.rect[3] * size[1].max(1) as f32];
    let values: [[f32; 4]; 8] = [
        scene.rect,
        [scene.mode as f32, pixels[0] / pixels[1].max(1.0), scene.night, scene.theta_max],
        [scene.center[0], scene.center[1], scene.half_extent[0], scene.half_extent[1]],
        [scene.camera[0], scene.camera[1], scene.camera[2], scene.opacity],
        [scene.disc[0], scene.disc[1], scene.disc[2], f32::from(u8::from(scene.cut_out))],
        [scene.fine[0] as f32, scene.fine[1] as f32, scene.fine[2] as f32, 0.0],
        [scene.coarse[0] as f32, scene.coarse[1] as f32, scene.coarse[2] as f32, 0.0],
        scene.turn,
    ];
    let mut bytes = [0; UNIFORM_BYTES];
    let all = values.iter().chain(scene.palette.iter()).flatten();
    for (chunk, value) in bytes.chunks_exact_mut(4).zip(all) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// The render thread. `try_lock`: if the game holds the scene this instant, this frame shows the last map.
pub fn draw(hand: &mut HandDrawn<'_>) {
    let Ok(mut shared) = SHARED.try_lock() else { return };
    let Some(scene) = shared.scene.as_ref() else { return };
    let params = uniforms(scene, hand.size);
    let Ok(mut state) = STATE.lock() else { return };
    if state.as_ref().is_none_or(|existing| existing.format != hand.color_format || existing.device != *hand.device) {
        *state = Some(build(hand.device, hand.color_format));
        // A new texture holds none of what the cache thinks it does. Happens at most when the frame format changes.
    }
    let Some(state) = state.as_ref() else { return };
    let count = shared.uploads.len().min(UPLOADS_PER_FRAME);
    for (slot, pixels) in shared.uploads.drain(..count) {
        write_layers(hand.queue, &state.tiles, u32::from(slot), 1, TILE_SIZE, 1, &pixels[..]);
    }
    if let Some(pages) = shared.pages.take() {
        write_layers(hand.queue, &state.pages, 0, 2, PAGE_SIZE, 4, &pages[..]);
    }
    drop(shared);
    hand.queue.write_buffer(&state.params, 0, &params);
    let mut pass = hand.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("vexmap map"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: hand.color_view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        ..Default::default()
    });
    pass.set_pipeline(&state.pipeline);
    pass.set_bind_group(0, &state.group, &[]);
    pass.draw(0..4, 0..1);
}

const SHADER: &str = r#"
struct Params {
    rect: vec4f,
    // mode (0 globe, 1 round, 2 flat), aspect, night, theta_max
    info: vec4f,
    // centre x, z; half extent x, z
    flat_view: vec4f,
    // yaw, pitch, distance, opacity
    camera: vec4f,
    // centre x, z; radius; 1 to leave what is outside the circle untouched
    disc: vec4f,
    // level, origin x, origin z
    fine: vec4f,
    coarse: vec4f,
    // Screen offset to world offset, screen y down: xx, xy, zx, zy.
    turn: vec4f,
    palette: array<vec4f, 16>,
}
@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var tiles: texture_2d_array<u32>;
@group(0) @binding(2) var pages: texture_2d_array<u32>;

const FOCAL: f32 = 2.2;
const PAGE: i32 = 64;

struct Out { @builtin(position) pos: vec4f, @location(0) uv: vec2f }

@vertex fn vs_main(@builtin(vertex_index) i: u32) -> Out {
    let corner = vec2f(f32(i & 1u), f32((i >> 1u) & 1u));
    let x = (p.rect.x + corner.x * p.rect.z) * 2.0 - 1.0;
    let y = 1.0 - (p.rect.y + corner.y * p.rect.w) * 2.0;
    return Out(vec4f(x, y, 0.0, 1.0), vec2f(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0));
}

fn texel(slot: i32, at: vec2i) -> vec4f {
    let raw = textureLoad(tiles, clamp(at, vec2i(0), vec2i(255)), slot, 0).r;
    let material = i32(raw & 15u);
    if (material == 0) { return vec4f(0.0); }
    // Shade 8 is the colour as chosen. Water and relief both move around it.
    let shade = 0.72 + f32(raw >> 4u) / 15.0 * 0.56;
    return vec4f(p.palette[material].rgb * shade, 1.0);
}

// A table entry for this world point, or nothing: xy tile uv in texels, z slot, w 1.
fn locate(world: vec2f, table: vec4f, layer: i32) -> vec4f {
    let span = 256.0 * pow(2.0, table.x);
    let cell = floor(world / span);
    let at = vec2i(cell) - vec2i(i32(table.y), i32(table.z));
    if (at.x < 0 || at.y < 0 || at.x >= PAGE || at.y >= PAGE) { return vec4f(0.0); }
    let entry = textureLoad(pages, at, layer, 0);
    if (entry.a == 0u) { return vec4f(0.0); }
    let up = span * pow(2.0, f32(entry.b));
    return vec4f(fract(world / up) * 256.0, f32(entry.r + entry.g * 256u), 1.0);
}

// Filtering after the palette, so materials blend at their edges and never turn into a third material.
fn ground(world: vec2f) -> vec4f {
    var found = locate(world, p.fine, 0);
    if (found.w == 0.0) { found = locate(world, p.coarse, 1); }
    if (found.w == 0.0) { return vec4f(0.0); }
    let slot = i32(found.z);
    let position = found.xy - 0.5;
    let base = floor(position);
    let f = position - base;
    let at = vec2i(base);
    let a = mix(texel(slot, at), texel(slot, at + vec2i(1, 0)), f.x);
    let b = mix(texel(slot, at + vec2i(0, 1)), texel(slot, at + vec2i(1, 1)), f.x);
    return mix(a, b, f.y);
}

fn surface(world: vec2f) -> vec3f {
    let ocean = p.palette[1].rgb * 0.82;
    let unknown = p.palette[0].rgb;
    let offset = world - p.disc.xy;
    let r = length(offset) / p.disc.z;
    if (r > 1.0) { return ocean; }
    let tile = ground(world);
    let rim = 1.0 - smoothstep(0.0, 0.004, 1.0 - r);
    return mix(mix(unknown, tile.rgb, tile.a), vec3f(1.0, 0.82, 0.32), rim);
}

fn space(uv: vec2f) -> vec3f {
    return mix(vec3f(0.60, 0.74, 0.90), vec3f(0.015, 0.02, 0.05), p.info.z) * (1.0 - 0.15 * length(uv));
}

fn from_sphere(n: vec3f) -> vec2f {
    let around = length(n.xz);
    if (around < 1e-5) { return p.disc.xy; }
    let distance = acos(clamp(n.y, -1.0, 1.0)) / p.info.w * p.disc.z;
    return p.disc.xy + n.xz / around * distance;
}

fn globe(uv: vec2f) -> vec3f {
    let dist = p.camera.z;
    let ro = vec3f(0.0, 0.0, -dist);
    let rd = normalize(vec3f(uv.x * p.info.y, uv.y, FOCAL));
    let bq = dot(ro, rd);
    let square = bq * bq - (dot(ro, ro) - 1.0);
    let miss = sqrt(max(dot(ro, ro) - bq * bq, 0.0));
    let halo = vec3f(0.25, 0.5, 1.0) * pow(clamp(1.08 - miss, 0.0, 1.0) / 0.08, 3.0) * step(miss, 1.08);
    if (square <= 0.0) { return space(uv) + halo; }
    let hit = ro + rd * (-bq - sqrt(square));
    let sp = sin(p.camera.y); let cp = cos(p.camera.y);
    let sy = sin(p.camera.x); let cy = cos(p.camera.x);
    let v = vec3f(hit.x, cp * hit.y - sp * hit.z, sp * hit.y + cp * hit.z);
    let n = vec3f(cy * v.x + sy * v.z, v.y, -sy * v.x + cy * v.z);
    let light = 0.55 + 0.55 * max(dot(hit, normalize(vec3f(0.5, 0.6, -0.7))), 0.0);
    let limb = pow(1.0 - max(dot(hit, -rd), 0.0), 3.0);
    return surface(from_sphere(n)) * light + vec3f(0.3, 0.55, 1.0) * limb * 0.5;
}

@fragment fn fs_main(in: Out) -> @location(0) vec4f {
    let mode = i32(p.info.x);
    if (mode == 0) { return vec4f(globe(in.uv), 1.0); }
    if (mode == 1) {
        let inscribed = in.uv * vec2f(max(p.info.y, 1.0), max(1.0 / p.info.y, 1.0));
        if (length(inscribed) > 1.0) {
            if (p.disc.w > 0.5) { discard; }
            return vec4f(space(in.uv), 1.0);
        }
    }
    // North (-Z) is up unless the minimap turns.
    let offset = vec2f(in.uv.x, -in.uv.y) * p.flat_view.zw;
    let world = p.flat_view.xy + vec2f(dot(p.turn.xy, offset), dot(p.turn.zw, offset));
    return vec4f(surface(world), p.camera.w);
}
"#;
