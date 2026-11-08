// Throwaway spike S2: can a client capture a cube panorama in the background without the player seeing it?
// One frame per face goes to a texture of its own through the renderer's scaled-screenshot path (the window keeps its last
// frame). The camera and field of view are overridden for that frame only, the face is copied before the HUD is drawn.
// Not for release. Writes PNG faces to VEXMAP_S2_DIR.
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::native_render::HandDrawn;

const FACES: [(&str, f32, f32); 6] = [
    ("south", 0.0, 0.0),
    ("west", 90.0, 0.0),
    ("north", 180.0, 0.0),
    ("east", 270.0, 0.0),
    ("up", 0.0, -90.0),
    ("down", 0.0, 90.0),
];
const SIDE: u32 = 256;
const INTERVAL: Duration = Duration::from_millis(3700);

struct Capture {
    position: [f64; 3],
    face: usize,
    frames_until_next: u32,
    active: bool,
    number: u32,
}

struct Pending {
    buffer: wgpu::Buffer,
    mapped: Arc<AtomicBool>,
    mapping: bool,
    name: String,
    format: wgpu::TextureFormat,
    frames: u32,
}

static AUTO: AtomicBool = AtomicBool::new(false);
static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
static POSE: Mutex<Option<([f64; 3], f32, f32)>> = Mutex::new(None);
static LAST: Mutex<Option<Instant>> = Mutex::new(None);
static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
static NUMBER: AtomicU32 = AtomicU32::new(0);
static PNG_BYTES: AtomicU32 = AtomicU32::new(0);

fn gap() -> u32 {
    std::env::var("VEXMAP_S2_GAP").ok().and_then(|v| v.parse().ok()).unwrap_or(10)
}

pub fn toggle_auto() {
    let on = !AUTO.fetch_xor(true, Ordering::Relaxed);
    eprintln!("[vexmap_s2] auto={on}");
}

/// Once a frame before anything is drawn: decides whether this frame is a face.
pub fn frame(eye: Option<[f64; 3]>) {
    let (Ok(mut capture), Ok(mut pose), Ok(mut last)) = (CAPTURE.lock(), POSE.lock(), LAST.lock()) else { return };
    *pose = None;
    if capture.is_none() {
        let due = last.is_none_or(|at| at.elapsed() >= INTERVAL);
        if AUTO.load(Ordering::Relaxed) && due && let Some(position) = eye {
            *last = Some(Instant::now());
            *capture = Some(Capture {
                position,
                face: 0,
                frames_until_next: 0,
                active: false,
                number: NUMBER.fetch_add(1, Ordering::Relaxed),
            });
        }
    }
    let Some(state) = capture.as_mut() else { return };
    state.active = false;
    if state.frames_until_next > 0 {
        state.frames_until_next -= 1;
        return;
    }
    if state.face == FACES.len() {
        *capture = None;
        return;
    }
    let (_, yaw, pitch) = FACES[state.face];
    *pose = Some((state.position, yaw, pitch));
    state.active = true;
}

pub fn capturing() -> bool {
    POSE.lock().is_ok_and(|pose| pose.is_some())
}

pub fn pose() -> Option<([f64; 3], f32, f32)> {
    POSE.lock().ok().and_then(|pose| *pose)
}

/// The size of the texture this frame goes to, when it is a face. The game builds its projection from the window's shape,
/// so the frame keeps that shape and a square is cut from its middle.
pub fn capture_size(window: [u32; 2]) -> Option<[u32; 2]> {
    capturing().then(|| {
        let width = (u64::from(SIDE) * u64::from(window[0]) / u64::from(window[1].max(1))) as u32;
        [width.max(SIDE), SIDE]
    })
}

fn output_dir() -> PathBuf {
    std::env::var_os("VEXMAP_S2_DIR").map_or_else(|| PathBuf::from("vexmap_s2"), PathBuf::from)
}

fn write_png(name: &str, width: u32, height: u32, format: wgpu::TextureFormat, data: &[u8]) -> Result<u32, String> {
    use wgpu::TextureFormat as F;
    let swap = matches!(format, F::Bgra8Unorm | F::Bgra8UnormSrgb);
    if !swap && !matches!(format, F::Rgba8Unorm | F::Rgba8UnormSrgb) {
        return Err(format!("format {format:?} is not handled by this spike"));
    }
    let mut pixels = data.to_vec();
    for pixel in pixels.chunks_exact_mut(4) {
        if swap {
            pixel.swap(0, 2);
        }
        pixel[3] = 255;
    }
    let dir = output_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{name}.png"));
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&pixels).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    std::fs::metadata(&path).map(|m| m.len() as u32).map_err(|e| e.to_string())
}

fn finish_mapped(device: &wgpu::Device) {
    let Ok(mut pending) = PENDING.lock() else { return };
    let mut index = 0;
    while index < pending.len() {
        let item = &mut pending[index];
        item.frames += 1;
        // The copy was submitted with the frame it belongs to, which has ended by the next call.
        if !item.mapping {
            item.mapping = true;
            let mapped = item.mapped.clone();
            item.buffer.map_async(wgpu::MapMode::Read, .., move |result| {
                mapped.store(result.is_ok(), Ordering::Release);
            });
        }
        if !item.mapped.load(Ordering::Acquire) {
            index += 1;
            continue;
        }
        let item = pending.remove(index);
        let Ok(view) = item.buffer.slice(..).get_mapped_range() else { continue };
        let data = view.to_vec();
        drop(view);
        let (name, format, frames) = (item.name, item.format, item.frames);
        std::thread::spawn(move || {
            let started = Instant::now();
            match write_png(&name, SIDE, SIDE, format, &data) {
                Ok(bytes) => {
                    let total = PNG_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
                    eprintln!(
                        "[vexmap_s2] face={name} png_kb={:.1} total_kb={:.1} encode_ms={} readback_frames={frames}",
                        f64::from(bytes) / 1024.0,
                        f64::from(total) / 1024.0,
                        started.elapsed().as_millis()
                    );
                }
                Err(e) => eprintln!("[vexmap_s2] face={name} failed: {e}"),
            }
        });
    }
    if !pending.is_empty() {
        let _ = device.poll(wgpu::PollType::Poll);
    }
}

struct Pacing {
    last_shown: Option<Instant>,
    gaps_ms: Vec<f32>,
    faces: u32,
    started: Instant,
}

static PACING: Mutex<Option<Pacing>> = Mutex::new(None);

/// Time between frames the player actually sees: a face's frame goes to a texture, so it is left out, and the gap it
/// leaves shows. Logged every 10 seconds, separately for capture on and off.
fn record_pacing(face_frame: bool) {
    let Ok(mut guard) = PACING.lock() else { return };
    let now = Instant::now();
    let pacing = guard.get_or_insert_with(|| Pacing { last_shown: None, gaps_ms: Vec::new(), faces: 0, started: now });
    if face_frame {
        pacing.faces += 1;
        return;
    }
    if let Some(last) = pacing.last_shown {
        pacing.gaps_ms.push((now - last).as_secs_f32() * 1000.0);
    }
    pacing.last_shown = Some(now);
    if pacing.started.elapsed() < Duration::from_secs(10) {
        return;
    }
    let mut gaps = std::mem::take(&mut pacing.gaps_ms);
    gaps.sort_by(f32::total_cmp);
    let at = |p: f32| gaps.get(((gaps.len().saturating_sub(1)) as f32 * p) as usize).copied().unwrap_or(0.0);
    let over = |limit: f32| gaps.iter().filter(|g| **g > limit).count();
    eprintln!(
        "[vexmap_s2] pacing auto={} faces={} shown={} gap_ms_p50={:.2} p99={:.2} max={:.2} over_12ms={} over_20ms={} over_40ms={}",
        AUTO.load(Ordering::Relaxed),
        pacing.faces,
        gaps.len(),
        at(0.5),
        at(0.99),
        at(1.0),
        over(12.0),
        over(20.0),
        over(40.0)
    );
    pacing.faces = 0;
    pacing.started = now;
}

/// After the world is drawn, before the HUD.
pub fn draw(hand: &mut HandDrawn<'_>) {
    finish_mapped(hand.device);
    let Ok(mut capture) = CAPTURE.lock() else { return };
    let face_frame = capture.as_ref().is_some_and(|state| state.active);
    record_pacing(face_frame);
    let Some(state) = capture.as_mut() else { return };
    if !state.active {
        return;
    }
    state.active = false;
    let size = hand.size;
    if size[1] != SIDE || size[0] < SIDE {
        eprintln!("[vexmap_s2] unexpected capture size {size:?}");
        return;
    }
    let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vexmap panorama face"),
        size: u64::from(SIDE * SIDE * 4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    hand.encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: hand.color,
            mip_level: 0,
            origin: wgpu::Origin3d { x: (size[0] - SIDE) / 2, y: 0, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIDE * 4),
                rows_per_image: Some(SIDE),
            },
        },
        wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
    );
    let name = format!("{:03}-{}", state.number, FACES[state.face].0);
    if let Ok(mut pending) = PENDING.lock() {
        pending.push(Pending {
            buffer,
            mapped: Arc::new(AtomicBool::new(false)),
            mapping: false,
            name,
            format: hand.color_format,
            frames: 0,
        });
    }
    state.face += 1;
    state.frames_until_next = gap();
}
