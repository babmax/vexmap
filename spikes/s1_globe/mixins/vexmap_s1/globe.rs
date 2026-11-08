// Throwaway spike S1: can a mod draw a full-screen globe over the world through `native_hand_drawn`, and what does it cost?
// Not for release: no tiles, placeholder terrain, unwrap-free but minimal. Logs frame times to stderr every 120 frames.
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use super::native_render::HandDrawn;

static VISIBLE: AtomicBool = AtomicBool::new(false);
static YAW: AtomicU32 = AtomicU32::new(0);
static PITCH: AtomicU32 = AtomicU32::new(0x3F00_0000); // 0.5 rad
static DISTANCE: AtomicU32 = AtomicU32::new(0x4040_0000); // 3.0 globe radii
static STATE: Mutex<Option<State>> = Mutex::new(None);
static STATS: Mutex<Option<Stats>> = Mutex::new(None);

// ADR-006: the world's disc covers this polar angle of the sphere. 120 degrees.
const THETA_MAX: f32 = 2.094_395;

struct State {
    format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    group: wgpu::BindGroup,
    params: wgpu::Buffer,
    started: Instant,
}

struct Stats {
    last: Instant,
    frames: u32,
    seconds: f32,
    worst: f32,
}

pub fn set_visible(visible: bool) {
    VISIBLE.store(visible, Ordering::Relaxed);
}

pub fn drag(delta: [f32; 2]) {
    let load = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
    let distance = load(&DISTANCE);
    let speed = 0.005 * (distance - 1.0).clamp(0.05, 2.0);
    YAW.store((load(&YAW) - delta[0] * speed).to_bits(), Ordering::Relaxed);
    let pitch = (load(&PITCH) + delta[1] * speed).clamp(-1.5, 1.5);
    PITCH.store(pitch.to_bits(), Ordering::Relaxed);
}

pub fn scroll(steps: f32) {
    let distance = f32::from_bits(DISTANCE.load(Ordering::Relaxed));
    let next = 1.0 + (distance - 1.0) * 0.9_f32.powf(steps);
    DISTANCE.store(next.clamp(1.02, 6.0).to_bits(), Ordering::Relaxed);
}

fn record_frame() {
    let Ok(mut stats) = STATS.lock() else { return };
    let now = Instant::now();
    let Some(stats) = stats.as_mut() else {
        *stats = Some(Stats { last: now, frames: 0, seconds: 0.0, worst: 0.0 });
        return;
    };
    let frame = (now - stats.last).as_secs_f32();
    stats.last = now;
    stats.frames += 1;
    stats.seconds += frame;
    stats.worst = stats.worst.max(frame);
    if stats.frames == 120 {
        let average = stats.seconds / 120.0;
        eprintln!(
            "[vexmap_s1] globe={} avg_ms={:.2} fps={:.1} worst_ms={:.2}",
            VISIBLE.load(Ordering::Relaxed),
            average * 1000.0,
            1.0 / average,
            stats.worst * 1000.0
        );
        stats.frames = 0;
        stats.seconds = 0.0;
        stats.worst = 0.0;
    }
}

fn build(device: &wgpu::Device, format: wgpu::TextureFormat) -> State {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("vexmap globe"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vexmap globe"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vexmap globe"),
        bind_group_layouts: &[Some(&layout)],
        ..Default::default()
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("vexmap globe"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let params = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vexmap globe"),
        size: 32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vexmap globe"),
        layout: &layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() }],
    });
    State { format, pipeline, group, params, started: Instant::now() }
}

pub fn draw(hand: &mut HandDrawn<'_>) {
    record_frame();
    if !VISIBLE.load(Ordering::Relaxed) {
        return;
    }
    let Ok(mut state) = STATE.lock() else { return };
    if state.as_ref().is_none_or(|s| s.format != hand.color_format) {
        *state = Some(build(hand.device, hand.color_format));
    }
    let Some(state) = state.as_ref() else { return };

    let load = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
    let values = [
        load(&YAW),
        load(&PITCH),
        load(&DISTANCE),
        hand.size[0] as f32 / hand.size[1].max(1) as f32,
        state.started.elapsed().as_secs_f32(),
        THETA_MAX,
        0.0,
        0.0,
    ];
    let mut bytes = [0u8; 32];
    for (chunk, value) in bytes.chunks_exact_mut(4).zip(values) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    hand.queue.write_buffer(&state.params, 0, &bytes);

    let mut pass = hand.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("vexmap globe"),
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
    pass.draw(0..3, 0..1);
}

const SHADER: &str = r#"
struct Params { a: vec4f, b: vec4f }
@group(0) @binding(0) var<uniform> p: Params;

struct Out { @builtin(position) pos: vec4f, @location(0) uv: vec2f }

@vertex fn vs_main(@builtin(vertex_index) i: u32) -> Out {
    let uv = vec2f(f32((i << 1u) & 2u), f32(i & 2u)) * 2.0 - 1.0;
    return Out(vec4f(uv, 0.0, 1.0), uv);
}

fn hash(q: vec2f) -> f32 {
    return fract(sin(dot(q, vec2f(127.1, 311.7))) * 43758.5453);
}
fn noise(q: vec2f) -> f32 {
    let i = floor(q);
    let f = fract(q);
    let u = f * f * (3.0 - 2.0 * f);
    return mix(mix(hash(i), hash(i + vec2f(1.0, 0.0)), u.x),
               mix(hash(i + vec2f(0.0, 1.0)), hash(i + vec2f(1.0, 1.0)), u.x), u.y);
}
fn fbm(q0: vec2f) -> f32 {
    var q = q0; var v = 0.0; var a = 0.5;
    for (var i = 0; i < 5; i++) { v += a * noise(q); q = q * 2.03 + 11.0; a *= 0.5; }
    return v;
}

// World disc coordinates, r = 1 at the rim: the azimuthal projection of ADR-006.
fn surface(n: vec3f) -> vec3f {
    let ocean = vec3f(0.11, 0.30, 0.52);
    let theta = acos(clamp(n.y, -1.0, 1.0));
    let r = theta / p.b.y;
    if (r >= 1.0) { return ocean * 0.8; }
    let lon = atan2(n.z, n.x);
    let w = r * vec2f(cos(lon), sin(lon));
    let h = fbm(w * 5.0 + 3.0) - 0.47 + 0.10 * (0.6 - r);
    var c = ocean;
    if (h > 0.0) { c = mix(vec3f(0.56, 0.72, 0.42), vec3f(0.80, 0.76, 0.58), clamp(h * 4.0, 0.0, 1.0)); }
    // Quadtree tile lines in world coordinates.
    let g = abs(fract(w * 8.0 + 0.5) - 0.5);
    let width = 0.01 + 0.03 * (p.a.z - 1.0); // Grows with the camera's distance; no derivatives in this branch.
    c = mix(c, vec3f(0.95), 0.18 * (1.0 - smoothstep(0.0, width, min(g.x, g.y))));
    // The rim of the world.
    c = mix(c, vec3f(1.0, 0.85, 0.3), 1.0 - smoothstep(0.0, 0.006, 1.0 - r));
    return c;
}

@fragment fn fs_main(in: Out) -> @location(0) vec4f {
    let yaw = p.a.x; let pitch = p.a.y; let dist = p.a.z; let aspect = p.a.w;
    let ro = vec3f(0.0, 0.0, -dist);
    let rd = normalize(vec3f(in.uv.x * aspect, in.uv.y, 2.2));
    let bq = dot(ro, rd);
    let disc = bq * bq - (dot(ro, ro) - 1.0);
    let miss = sqrt(max(dot(ro, ro) - bq * bq, 0.0));
    var color = vec3f(0.015, 0.02, 0.05) + vec3f(0.25, 0.5, 1.0) * pow(clamp(1.08 - miss, 0.0, 1.0) / 0.08, 3.0) * step(miss, 1.08);
    if (disc > 0.0) {
        let hit = ro + rd * (-bq - sqrt(disc));
        let cp = cos(pitch); let sp = sin(pitch);
        let cy = cos(yaw); let sy = sin(yaw);
        let v = vec3f(hit.x, cp * hit.y - sp * hit.z, sp * hit.y + cp * hit.z);
        let n = vec3f(cy * v.x + sy * v.z, v.y, -sy * v.x + cy * v.z);
        let light = 0.35 + 0.75 * max(dot(hit, normalize(vec3f(0.5, 0.6, -0.7))), 0.0);
        let limb = pow(1.0 - max(dot(hit, -rd), 0.0), 3.0);
        color = surface(n) * light + vec3f(0.3, 0.55, 1.0) * limb * 0.6;
    }
    return vec4f(color, 1.0);
}
"#;
