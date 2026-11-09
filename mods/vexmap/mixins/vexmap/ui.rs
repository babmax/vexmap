// VexMap's client interface without the game: settings and presets, the map's view, and what to draw as a list of `Op`s.
// Only `std`, so it is also the `vexmap-ui` crate and is tested and measured without building Horizon. `map_screen.rs`
// replays the lists through Horizon's GUI helpers.
//
// The minimap is still schematic: land, grid and players. The map screen draws its ground through the GPU pass in render.rs.

use std::collections::{HashMap, HashSet};

/// Until Phase 5 reads the world border.
pub const WORLD_RADIUS: f64 = 16_384.0;
pub const SETTINGS_FILE: &str = "vexmap.cfg";
/// How many other players are drawn at most. The rest are in the table, and clusters stand in for them on far zooms.
pub const MAX_PINS: usize = 5000;

const MIN_BLOCKS_PER_PIXEL: f64 = 1.0 / 16.0;
const MAX_BLOCKS_PER_PIXEL: f64 = 256.0;
const MIN_WINDOW: [f32; 2] = [200.0, 140.0];
const TITLE_BAR: f32 = 16.0;
const GRIP: f32 = 10.0;
const ROW_HEIGHT: f32 = 16.0;
const SAVE_AFTER_MS: u64 = 500;
/// The most a player can give the tile cache.
pub const MAX_TILE_CACHE_MB: u32 = 4096;
const AUTOTUNE_MS: u64 = 3_000;

const OCEAN: u32 = 0xFF1B_3A5B;
const LAND: u32 = 0xFFCF_E3B4;
const GRID: u32 = 0x3000_0000;
const RIM: u32 = 0xFFFF_D166;
const PANEL: u32 = 0xE010_1820;
const BUTTON: u32 = 0xFF2A_3644;
const BUTTON_HOVER: u32 = 0xFF3C_5068;
const WHITE: u32 = 0x00FF_FFFF;
const MUTED: u32 = 0x00B8_C4D0;
const OTHER_PLAYER: u32 = 0xFF35_D0A5;
const SELF_PLAYER: u32 = 0xFFE5_4B3C;
const GROUP: u32 = 0xFFFF_B547;

/// Text in both languages: the translation key is the pair itself.
pub type Tr = (&'static str, &'static str);

macro_rules! choice {
    ($name:ident { $($variant:ident = $text:literal, $en:literal, $ru:literal);+ $(;)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            const ALL: &'static [$name] = &[$($name::$variant),+];

            fn text(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }

            fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some($name::$variant),)+ _ => None }
            }

            pub fn label(self) -> Tr {
                match self { $($name::$variant => ($en, $ru)),+ }
            }

            pub fn next(self) -> Self {
                let at = Self::ALL.iter().position(|value| *value == self).unwrap_or(0);
                Self::ALL[(at + 1) % Self::ALL.len()]
            }
        }
    };
}

choice!(Language { En = "en", "English", "English"; Ru = "ru", "Русский", "Русский" });
choice!(Preset {
    Beauty = "beauty", "Beauty", "Красота";
    Balance = "balance", "Balance", "Баланс";
    Performance = "performance", "Performance", "Производительность";
    Roleplay = "rp", "RP", "RP";
    Gps = "gps", "GPS", "GPS";
    Custom = "custom", "Custom", "Свой";
});
choice!(Quality { Low = "low", "Low", "Низкое"; Medium = "medium", "Medium", "Среднее"; High = "high", "High", "Высокое" });
choice!(Labels { Few = "few", "Few", "Мало"; Normal = "normal", "Normal", "Обычно"; Many = "many", "Many", "Много" });
choice!(Corner {
    TopLeft = "top_left", "Top left", "Слева сверху";
    TopRight = "top_right", "Top right", "Справа сверху";
    BottomLeft = "bottom_left", "Bottom left", "Слева снизу";
    BottomRight = "bottom_right", "Bottom right", "Справа снизу";
});
choice!(Shape { Circle = "circle", "Circle", "Круг"; Square = "square", "Square", "Квадрат" });
choice!(MapMode { Globe = "globe", "Globe", "Глобус"; Round = "round", "Round", "Круг"; Flat = "flat", "Flat", "Плоская" });
choice!(WindowMode { Fullscreen = "fullscreen", "Full screen", "Весь экран"; Windowed = "windowed", "Window", "Окно" });

impl Language {
    pub fn pick(self, text: Tr) -> &'static str {
        match self {
            Language::En => text.0,
            Language::Ru => text.1,
        }
    }
}

/// Map layers as bits of `Settings::layers`. New layers take new bits, so old files stay valid.
pub mod layer {
    pub const GRID: u32 = 1 << 0;
    pub const PLAYERS: u32 = 1 << 1;
    pub const CLUSTERS: u32 = 1 << 2;
    pub const COUNTRIES: u32 = 1 << 3;
    pub const DIPLOMACY: u32 = 1 << 4;
    pub const FRONT: u32 = 1 << 5;
    pub const BASIC: u32 = GRID | PLAYERS | CLUSTERS;
    pub const MINIMAP_ALL: u32 = GRID | PLAYERS | CLUSTERS;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Minimap {
    pub enabled: bool,
    pub corner: Corner,
    /// Side in GUI pixels.
    pub size: u16,
    pub shape: Shape,
    /// World blocks per GUI pixel.
    pub zoom: f32,
    pub rotate_with_look: bool,
    /// 10 to 100.
    pub opacity: u8,
    pub layers: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub language: Language,
    pub preset: Preset,
    pub quality: Quality,
    pub buildings_3d: bool,
    pub effects: bool,
    pub labels: Labels,
    /// How often the map's and minimap's draw lists are rebuilt.
    pub update_hz: u8,
    pub layers: u32,
    pub minimap: Minimap,
    pub window_mode: WindowMode,
    pub map_mode: MapMode,
    /// `[x, y, w, h]` of the windowed map in GUI pixels.
    pub window: [f32; 4],
    pub key_map: String,
    pub key_minimap: String,
    pub autotuned: bool,
    /// Disk the tile cache may use, in MiB. 0 turns the disk cache off.
    pub tile_cache_mb: u32,
}

impl Default for Settings {
    fn default() -> Self {
        let mut settings = Settings {
            language: Language::Ru,
            preset: Preset::Balance,
            quality: Quality::Medium,
            buildings_3d: true,
            effects: false,
            labels: Labels::Normal,
            update_hz: 20,
            layers: layer::BASIC,
            minimap: Minimap {
                enabled: true,
                corner: Corner::TopRight,
                size: 96,
                shape: Shape::Circle,
                zoom: 2.0,
                rotate_with_look: false,
                opacity: 90,
                layers: layer::MINIMAP_ALL,
            },
            window_mode: WindowMode::Fullscreen,
            map_mode: MapMode::Globe,
            window: [40.0, 30.0, 400.0, 260.0],
            key_map: "key.keyboard.m".to_owned(),
            key_minimap: "key.keyboard.n".to_owned(),
            autotuned: false,
            tile_cache_mb: 256,
        };
        settings.apply_preset(Preset::Balance);
        settings
    }
}

fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

impl Settings {
    /// What a preset controls. `Custom` leaves everything as it is. Quality, 3D buildings and effects are read by the
    /// renderers of Phases 5 and 9; they live here so the file format and the presets do not change then.
    pub fn apply_preset(&mut self, preset: Preset) {
        self.preset = preset;
        let (quality, buildings, effects, labels, hz, layers) = match preset {
            Preset::Beauty => (Quality::High, true, true, Labels::Many, 30, layer::BASIC),
            Preset::Balance => (Quality::Medium, true, false, Labels::Normal, 20, layer::BASIC),
            Preset::Performance => (Quality::Low, false, false, Labels::Few, 10, layer::PLAYERS | layer::CLUSTERS),
            Preset::Roleplay => (
                Quality::Medium,
                true,
                false,
                Labels::Normal,
                20,
                layer::BASIC | layer::COUNTRIES | layer::DIPLOMACY | layer::FRONT,
            ),
            Preset::Gps => (Quality::Low, false, false, Labels::Few, 20, layer::PLAYERS),
            Preset::Custom => return,
        };
        self.quality = quality;
        self.buildings_3d = buildings;
        self.effects = effects;
        self.labels = labels;
        self.update_hz = hz;
        self.layers = layers;
        if preset == Preset::Gps {
            self.minimap.enabled = true;
            self.minimap.size = 128;
            self.minimap.rotate_with_look = true;
        }
    }

    /// `key=value` lines. A file that does not parse never stops the game: each bad line just keeps its default.
    pub fn parse(text: &str) -> Settings {
        let mut settings = Settings::default();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                settings.set(key.trim(), value.trim());
            }
        }
        settings.sanitize();
        settings
    }

    fn set(&mut self, key: &str, value: &str) -> Option<()> {
        match key {
            "language" => self.language = Language::parse(value)?,
            "preset" => self.preset = Preset::parse(value)?,
            "quality" => self.quality = Quality::parse(value)?,
            "buildings_3d" => self.buildings_3d = parse_bool(value)?,
            "effects" => self.effects = parse_bool(value)?,
            "labels" => self.labels = Labels::parse(value)?,
            "update_hz" => self.update_hz = value.parse().ok()?,
            "layers" => self.layers = value.parse().ok()?,
            "minimap.enabled" => self.minimap.enabled = parse_bool(value)?,
            "minimap.corner" => self.minimap.corner = Corner::parse(value)?,
            "minimap.size" => self.minimap.size = value.parse().ok()?,
            "minimap.shape" => self.minimap.shape = Shape::parse(value)?,
            "minimap.zoom" => self.minimap.zoom = value.parse().ok()?,
            "minimap.rotate_with_look" => self.minimap.rotate_with_look = parse_bool(value)?,
            "minimap.opacity" => self.minimap.opacity = value.parse().ok()?,
            "minimap.layers" => self.minimap.layers = value.parse().ok()?,
            "window_mode" => self.window_mode = WindowMode::parse(value)?,
            "map_mode" => self.map_mode = MapMode::parse(value)?,
            "window" => {
                let mut parts = value.split(',').map(|part| part.trim().parse::<f32>().ok());
                let mut window = [0.0; 4];
                for slot in &mut window {
                    *slot = parts.next()??;
                }
                self.window = window;
            }
            "key_map" => self.key_map = value.to_owned(),
            "key_minimap" => self.key_minimap = value.to_owned(),
            "autotuned" => self.autotuned = parse_bool(value)?,
            "tile_cache_mb" => self.tile_cache_mb = value.parse().ok()?,
            _ => {}
        }
        Some(())
    }

    fn sanitize(&mut self) {
        self.update_hz = self.update_hz.clamp(1, 60);
        self.tile_cache_mb = self.tile_cache_mb.min(MAX_TILE_CACHE_MB);
        self.minimap.size = self.minimap.size.clamp(48, 256);
        self.minimap.opacity = self.minimap.opacity.clamp(10, 100);
        if !self.minimap.zoom.is_finite() {
            self.minimap.zoom = 2.0;
        }
        self.minimap.zoom = self.minimap.zoom.clamp(0.25, 32.0);
        if self.window.iter().any(|value| !value.is_finite()) {
            self.window = Settings::default().window;
        }
    }

    pub fn to_text(&self) -> String {
        let minimap = &self.minimap;
        let [x, y, w, h] = self.window;
        format!(
            "language={}\npreset={}\nquality={}\nbuildings_3d={}\neffects={}\nlabels={}\nupdate_hz={}\nlayers={}\n\
             minimap.enabled={}\nminimap.corner={}\nminimap.size={}\nminimap.shape={}\nminimap.zoom={}\n\
             minimap.rotate_with_look={}\nminimap.opacity={}\nminimap.layers={}\nwindow_mode={}\nmap_mode={}\nwindow={x},{y},{w},{h}\n\
             key_map={}\nkey_minimap={}\nautotuned={}\ntile_cache_mb={}\n",
            self.language.text(),
            self.preset.text(),
            self.quality.text(),
            self.buildings_3d,
            self.effects,
            self.labels.text(),
            self.update_hz,
            self.layers,
            minimap.enabled,
            minimap.corner.text(),
            minimap.size,
            minimap.shape.text(),
            minimap.zoom,
            minimap.rotate_with_look,
            minimap.opacity,
            minimap.layers,
            self.window_mode.text(),
            self.map_mode.text(),
            self.key_map,
            self.key_minimap,
            self.autotuned,
            self.tile_cache_mb,
        )
    }
}

/// One drawing call. `rgb` is opaque, `argb` carries alpha, as Horizon's helpers want.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Fill {
        rect: [f32; 4],
        argb: u32,
    },
    Quad {
        points: [[f32; 2]; 4],
        argb: u32,
    },
    Text {
        text: String,
        at: [f32; 2],
        rgb: u32,
    },
    /// Centred on `at.0`, so it needs the game's text measure.
    TextCentered {
        text: String,
        at: [f32; 2],
        rgb: u32,
    },
    Sprite {
        name: &'static str,
        rect: [f32; 4],
    },
}

/// Another player on the map, already in blocks.
#[derive(Clone, Copy, Debug)]
pub struct Pin<'a> {
    pub uuid: u128,
    pub name: &'a str,
    pub dimension: &'a str,
    pub x: f64,
    pub z: f64,
}

/// A cluster of players.
#[derive(Clone, Copy, Debug)]
pub struct Group<'a> {
    pub dimension: &'a str,
    pub x: f64,
    pub z: f64,
    pub count: u16,
}

/// The local player, read from the client and never asked of the server.
#[derive(Clone, Debug, PartialEq)]
pub struct Player {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// Degrees. 0 faces south, 90 west.
    pub yaw: f32,
    pub dimension: String,
}

/// Which part of the world the map shows. North (-Z) is up. Rendering only: distances stay in world coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapView {
    pub center: [f64; 2],
    pub blocks_per_pixel: f64,
    /// The circle the world is cut to, from the server.
    pub disc: Disc,
}

impl Default for MapView {
    fn default() -> Self {
        MapView { center: [0.0, 0.0], blocks_per_pixel: 2.0, disc: Disc::default() }
    }
}

fn rect_center(rect: [f32; 4]) -> [f32; 2] {
    [rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0]
}

impl MapView {
    pub fn world_to_screen(&self, rect: [f32; 4], world: [f64; 2]) -> [f32; 2] {
        let center = rect_center(rect);
        [
            center[0] + ((world[0] - self.center[0]) / self.blocks_per_pixel) as f32,
            center[1] + ((world[1] - self.center[1]) / self.blocks_per_pixel) as f32,
        ]
    }

    pub fn screen_to_world(&self, rect: [f32; 4], screen: [f32; 2]) -> [f64; 2] {
        let center = rect_center(rect);
        [
            self.center[0] + f64::from(screen[0] - center[0]) * self.blocks_per_pixel,
            self.center[1] + f64::from(screen[1] - center[1]) * self.blocks_per_pixel,
        ]
    }

    fn clamp_center(&mut self) {
        let (x, z) = (self.center[0] - self.disc.center[0], self.center[1] - self.disc.center[1]);
        let distance = x.hypot(z);
        if distance > self.disc.radius {
            let scale = self.disc.radius / distance;
            self.center = [self.disc.center[0] + x * scale, self.disc.center[1] + z * scale];
        }
    }

    pub fn pan(&mut self, delta: [f32; 2]) {
        self.center[0] -= f64::from(delta[0]) * self.blocks_per_pixel;
        self.center[1] -= f64::from(delta[1]) * self.blocks_per_pixel;
        self.clamp_center();
    }

    /// The world point under the cursor stays under it.
    pub fn zoom_at(&mut self, rect: [f32; 4], cursor: [f32; 2], steps: f64) {
        let before = self.screen_to_world(rect, cursor);
        self.blocks_per_pixel =
            (self.blocks_per_pixel * 0.85_f64.powf(steps)).clamp(MIN_BLOCKS_PER_PIXEL, MAX_BLOCKS_PER_PIXEL);
        let after = self.screen_to_world(rect, cursor);
        self.center[0] += before[0] - after[0];
        self.center[1] += before[1] - after[1];
        self.clamp_center();
    }
}

#[derive(Clone, Copy)]
enum Clip {
    Rect([f32; 4]),
    Circle([f32; 2], f32),
}

impl Clip {
    fn contains(self, point: [f32; 2]) -> bool {
        match self {
            Clip::Rect([x, y, w, h]) => point[0] >= x && point[0] <= x + w && point[1] >= y && point[1] <= y + h,
            Clip::Circle(center, radius) => {
                (point[0] - center[0]).powi(2) + (point[1] - center[1]).powi(2) <= radius * radius
            }
        }
    }

    /// The part of the segment `a + t (b - a)`, `t` in 0..1, inside.
    fn segment(self, a: [f32; 2], b: [f32; 2]) -> Option<(f32, f32)> {
        let d = [b[0] - a[0], b[1] - a[1]];
        match self {
            Clip::Rect([x, y, w, h]) => {
                let (mut t0, mut t1) = (0.0_f32, 1.0_f32);
                for (p, q) in [(-d[0], a[0] - x), (d[0], x + w - a[0]), (-d[1], a[1] - y), (d[1], y + h - a[1])] {
                    if p == 0.0 {
                        if q < 0.0 {
                            return None;
                        }
                        continue;
                    }
                    let t = q / p;
                    if p < 0.0 {
                        t0 = t0.max(t);
                    } else {
                        t1 = t1.min(t);
                    }
                }
                (t0 <= t1).then_some((t0, t1))
            }
            Clip::Circle(center, radius) => {
                let f = [a[0] - center[0], a[1] - center[1]];
                let (qa, qb, qc) =
                    (d[0] * d[0] + d[1] * d[1], f[0] * d[0] + f[1] * d[1], f[0] * f[0] + f[1] * f[1] - radius * radius);
                if qa == 0.0 {
                    return (qc <= 0.0).then_some((0.0, 1.0));
                }
                let disc = qb * qb - qa * qc;
                if disc < 0.0 {
                    return None;
                }
                let root = disc.sqrt();
                let (t0, t1) = (((-qb - root) / qa).max(0.0), ((-qb + root) / qa).min(1.0));
                (t0 <= t1).then_some((t0, t1))
            }
        }
    }
}

fn with_alpha(argb: u32, percent: u8) -> u32 {
    let alpha = (argb >> 24) * u32::from(percent) / 100;
    (argb & 0x00FF_FFFF) | (alpha << 24)
}

fn line(out: &mut Vec<Op>, clips: &[Clip], a: [f32; 2], b: [f32; 2], width: f32, argb: u32) {
    let (mut t0, mut t1) = (0.0_f32, 1.0_f32);
    for clip in clips {
        let Some((from, to)) = clip.segment(a, b) else { return };
        t0 = t0.max(from);
        t1 = t1.min(to);
    }
    if t0 >= t1 {
        return;
    }
    let d = [b[0] - a[0], b[1] - a[1]];
    let length = d[0].hypot(d[1]);
    if length == 0.0 {
        return;
    }
    let normal = [-d[1] / length * width / 2.0, d[0] / length * width / 2.0];
    let p = [a[0] + d[0] * t0, a[1] + d[1] * t0];
    let q = [a[0] + d[0] * t1, a[1] + d[1] * t1];
    out.push(Op::Quad {
        points: [
            [p[0] + normal[0], p[1] + normal[1]],
            [q[0] + normal[0], q[1] + normal[1]],
            [q[0] - normal[0], q[1] - normal[1]],
            [p[0] - normal[0], p[1] - normal[1]],
        ],
        argb,
    });
}

/// A triangle cut down to `rect` (Sutherland-Hodgman), as quads from its first corner.
fn clipped_triangle(out: &mut Vec<Op>, rect: [f32; 4], triangle: [[f32; 2]; 3], argb: u32) {
    let [x, y, w, h] = rect;
    let mut polygon: Vec<[f32; 2]> = triangle.to_vec();
    let edges: [(usize, f32, f32); 4] = [(0, x, 1.0), (0, x + w, -1.0), (1, y, 1.0), (1, y + h, -1.0)];
    for (axis, limit, sign) in edges {
        let mut next = Vec::with_capacity(polygon.len() + 1);
        for (index, current) in polygon.iter().enumerate() {
            let previous = polygon[(index + polygon.len() - 1) % polygon.len()];
            let (cd, pd) = ((current[axis] - limit) * sign, (previous[axis] - limit) * sign);
            if (cd >= 0.0) != (pd >= 0.0) {
                let t = pd / (pd - cd);
                next.push([previous[0] + (current[0] - previous[0]) * t, previous[1] + (current[1] - previous[1]) * t]);
            }
            if cd >= 0.0 {
                next.push(*current);
            }
        }
        polygon = next;
        if polygon.is_empty() {
            return;
        }
    }
    let first = polygon[0];
    for pair in polygon[1..].windows(2) {
        out.push(Op::Quad { points: [first, pair[0], pair[1], pair[1]], argb });
    }
}

fn disc(out: &mut Vec<Op>, center: [f32; 2], radius: f32, clip: Option<[f32; 4]>, argb: u32) {
    let segments = (radius * 0.5).clamp(32.0, 720.0) as usize;
    let at = |index: usize| {
        let angle = index as f32 / segments as f32 * std::f32::consts::TAU;
        [center[0] + angle.cos() * radius, center[1] + angle.sin() * radius]
    };
    for index in 0..segments {
        let (a, b) = (at(index), at(index + 1));
        match clip {
            Some(rect) => clipped_triangle(out, rect, [center, a, b], argb),
            None => out.push(Op::Quad { points: [center, a, b, b], argb }),
        }
    }
}

fn ring(out: &mut Vec<Op>, clips: &[Clip], center: [f32; 2], radius: f32, width: f32, argb: u32) {
    let segments = (radius * 0.5).clamp(32.0, 720.0) as usize;
    let at = |index: usize| {
        let angle = index as f32 / segments as f32 * std::f32::consts::TAU;
        [center[0] + angle.cos() * radius, center[1] + angle.sin() * radius]
    };
    for index in 0..segments {
        line(out, clips, at(index), at(index + 1), width, argb);
    }
}

fn arrow(out: &mut Vec<Op>, at: [f32; 2], heading: [f32; 2], size: f32, argb: u32) {
    let side = [-heading[1], heading[0]];
    let point = |along: f32, across: f32| {
        [
            at[0] + heading[0] * along * size + side[0] * across * size,
            at[1] + heading[1] * along * size + side[1] * across * size,
        ]
    };
    out.push(Op::Quad { points: [point(1.0, 0.0), point(-0.7, 0.65), point(-0.3, 0.0), point(-0.7, -0.65)], argb });
}

/// Where the local player looks, as a map direction: X to the right, Z down.
fn yaw_vector(yaw_degrees: f32) -> [f32; 2] {
    let radians = yaw_degrees.to_radians();
    [-radians.sin(), radians.cos()]
}

fn marker(out: &mut Vec<Op>, at: [f32; 2], size: f32, argb: u32) {
    let half = size / 2.0;
    out.push(Op::Fill { rect: [at[0] - half - 1.0, at[1] - half - 1.0, size + 2.0, size + 2.0], argb: 0xFF10_1820 });
    out.push(Op::Fill { rect: [at[0] - half, at[1] - half, size, size], argb });
}

fn format_distance(language: Language, blocks: f64) -> String {
    if blocks >= 1000.0 {
        format!("{:.1} {}", blocks / 1000.0, language.pick(("km", "км")))
    } else {
        format!("{blocks:.0} {}", language.pick(("m", "м")))
    }
}

fn grid_step(blocks_per_pixel: f64) -> f64 {
    let mut step = 16.0;
    while step / blocks_per_pixel < 48.0 {
        step *= 2.0;
    }
    step
}

fn label_budget(labels: Labels) -> usize {
    match labels {
        Labels::Few => 16,
        Labels::Normal => 64,
        Labels::Many => 256,
    }
}

/// One entry of the settings page: what it shows and what a click does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Language,
    Preset,
    Quality,
    Buildings,
    Effects,
    Labels,
    UpdateRate,
    Grid,
    Players,
    Clusters,
    Countries,
    Diplomacy,
    Front,
    WindowMode,
    KeyMap,
    KeyMinimap,
    MinimapOn,
    MinimapCorner,
    MinimapSize,
    MinimapShape,
    MinimapZoom,
    MinimapRotate,
    MinimapOpacity,
    MinimapGrid,
    MinimapPlayers,
    MinimapClusters,
}

const LEFT_ROWS: [Row; 16] = [
    Row::Language,
    Row::Preset,
    Row::Quality,
    Row::Buildings,
    Row::Effects,
    Row::Labels,
    Row::UpdateRate,
    Row::Grid,
    Row::Players,
    Row::Clusters,
    Row::Countries,
    Row::Diplomacy,
    Row::Front,
    Row::WindowMode,
    Row::KeyMap,
    Row::KeyMinimap,
];
const RIGHT_ROWS: [Row; 10] = [
    Row::MinimapOn,
    Row::MinimapCorner,
    Row::MinimapSize,
    Row::MinimapShape,
    Row::MinimapZoom,
    Row::MinimapRotate,
    Row::MinimapOpacity,
    Row::MinimapGrid,
    Row::MinimapPlayers,
    Row::MinimapClusters,
];

fn on_off(language: Language, value: bool) -> String {
    language.pick(if value { ("On", "Вкл") } else { ("Off", "Выкл") }).to_owned()
}

/// `key.keyboard.m` to `M`, for the settings page.
fn key_label(key: &str) -> String {
    key.rsplit('.').next().unwrap_or(key).to_uppercase()
}

fn next_in<T: Copy + PartialEq>(values: &[T], current: T) -> T {
    let at = values.iter().position(|value| *value == current).map_or(0, |at| (at + 1) % values.len());
    values[at]
}

impl Row {
    fn label(self) -> Tr {
        match self {
            Row::Language => ("Language", "Язык"),
            Row::Preset => ("Preset", "Пресет"),
            Row::Quality => ("Quality", "Качество"),
            Row::Buildings => ("3D buildings", "3D-здания"),
            Row::Effects => ("Effects", "Эффекты"),
            Row::Labels => ("Labels", "Подписи"),
            Row::UpdateRate => ("Update rate", "Частота обновления"),
            Row::Grid | Row::MinimapGrid => ("Grid", "Сетка"),
            Row::Players | Row::MinimapPlayers => ("Players", "Игроки"),
            Row::Clusters | Row::MinimapClusters => ("Clusters", "Кластеры"),
            Row::Countries => ("Countries", "Страны"),
            Row::Diplomacy => ("Diplomacy", "Дипломатия"),
            Row::Front => ("Front line", "Линия фронта"),
            Row::WindowMode => ("Map window", "Окно карты"),
            Row::KeyMap => ("Map key", "Клавиша карты"),
            Row::KeyMinimap => ("Minimap key", "Клавиша миникарты"),
            Row::MinimapOn => ("Minimap", "Миникарта"),
            Row::MinimapCorner => ("Position", "Положение"),
            Row::MinimapSize => ("Size", "Размер"),
            Row::MinimapShape => ("Shape", "Форма"),
            Row::MinimapZoom => ("Scale", "Масштаб"),
            Row::MinimapRotate => ("Rotate with view", "Поворот по взгляду"),
            Row::MinimapOpacity => ("Opacity", "Прозрачность"),
        }
    }

    fn layer_bit(self) -> Option<(bool, u32)> {
        Some(match self {
            Row::Grid => (false, layer::GRID),
            Row::Players => (false, layer::PLAYERS),
            Row::Clusters => (false, layer::CLUSTERS),
            Row::Countries => (false, layer::COUNTRIES),
            Row::Diplomacy => (false, layer::DIPLOMACY),
            Row::Front => (false, layer::FRONT),
            Row::MinimapGrid => (true, layer::GRID),
            Row::MinimapPlayers => (true, layer::PLAYERS),
            Row::MinimapClusters => (true, layer::CLUSTERS),
            _ => return None,
        })
    }

    fn value(self, settings: &Settings) -> String {
        let language = settings.language;
        let label = |text: Tr| language.pick(text).to_owned();
        let minimap = &settings.minimap;
        if let Some((mini, bit)) = self.layer_bit() {
            return on_off(language, (if mini { minimap.layers } else { settings.layers }) & bit != 0);
        }
        match self {
            Row::Language => label(settings.language.label()),
            Row::Preset => label(settings.preset.label()),
            Row::Quality => label(settings.quality.label()),
            Row::Buildings => on_off(language, settings.buildings_3d),
            Row::Effects => on_off(language, settings.effects),
            Row::Labels => label(settings.labels.label()),
            Row::UpdateRate => format!("{} Hz", settings.update_hz),
            Row::WindowMode => label(settings.window_mode.label()),
            Row::KeyMap => key_label(&settings.key_map),
            Row::KeyMinimap => key_label(&settings.key_minimap),
            Row::MinimapOn => on_off(language, minimap.enabled),
            Row::MinimapCorner => label(minimap.corner.label()),
            Row::MinimapSize => minimap.size.to_string(),
            Row::MinimapShape => label(minimap.shape.label()),
            Row::MinimapZoom => format!("{}", minimap.zoom),
            Row::MinimapRotate => on_off(language, minimap.rotate_with_look),
            Row::MinimapOpacity => format!("{}%", minimap.opacity),
            _ => String::new(),
        }
    }

    /// A click. Anything a preset also sets turns the preset into `Custom`, so the page never claims a preset that is not in force.
    fn activate(self, settings: &mut Settings) {
        if let Some((mini, bit)) = self.layer_bit() {
            if mini {
                settings.minimap.layers ^= bit;
            } else {
                settings.layers ^= bit;
                settings.preset = Preset::Custom;
            }
            return;
        }
        let minimap = &mut settings.minimap;
        match self {
            Row::Language => settings.language = settings.language.next(),
            Row::Preset => {
                let next = settings.preset.next();
                settings.apply_preset(next);
            }
            Row::WindowMode => settings.window_mode = settings.window_mode.next(),
            Row::MinimapOn => minimap.enabled = !minimap.enabled,
            Row::MinimapCorner => minimap.corner = minimap.corner.next(),
            Row::MinimapSize => minimap.size = next_in(&[64, 96, 128, 192], minimap.size),
            Row::MinimapShape => minimap.shape = minimap.shape.next(),
            Row::MinimapZoom => minimap.zoom = next_in(&[0.5, 1.0, 2.0, 4.0, 8.0], minimap.zoom),
            Row::MinimapRotate => minimap.rotate_with_look = !minimap.rotate_with_look,
            Row::MinimapOpacity => minimap.opacity = next_in(&[100, 90, 75, 50, 25], minimap.opacity),
            Row::Quality => {
                settings.quality = settings.quality.next();
                settings.preset = Preset::Custom;
            }
            Row::Buildings => {
                settings.buildings_3d = !settings.buildings_3d;
                settings.preset = Preset::Custom;
            }
            Row::Effects => {
                settings.effects = !settings.effects;
                settings.preset = Preset::Custom;
            }
            Row::Labels => {
                settings.labels = settings.labels.next();
                settings.preset = Preset::Custom;
            }
            Row::UpdateRate => {
                settings.update_hz = next_in(&[10, 20, 30, 60], settings.update_hz);
                settings.preset = Preset::Custom;
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Map,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grab {
    Pan,
    Move,
    Resize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Autotune {
    Off,
    Offer,
    Measuring { until_ms: u64, total: u64, samples: u32, next_ms: u64 },
}

/// The preset for a measured frame rate. The thresholds are a first guess, to be set against the baseline machine.
pub fn preset_for_fps(fps: f64) -> Preset {
    if fps >= 90.0 {
        Preset::Beauty
    } else if fps >= 45.0 {
        Preset::Balance
    } else {
        Preset::Performance
    }
}

/// What a press means for the screen around the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Ignored,
    Taken,
    Close,
}

fn inside(rect: [f32; 4], point: [f32; 2]) -> bool {
    Clip::Rect(rect).contains(point)
}

fn button(out: &mut Vec<Op>, rect: [f32; 4], text: &str, mouse: [f32; 2]) {
    out.push(Op::Fill { rect, argb: if inside(rect, mouse) { BUTTON_HOVER } else { BUTTON } });
    out.push(Op::TextCentered {
        text: text.to_owned(),
        at: [rect[0] + rect[2] / 2.0, rect[1] + (rect[3] - 8.0) / 2.0],
        rgb: WHITE,
    });
}

/// The small button that opens the map from the pause menu: top left, away from the menu's centred column.
pub fn pause_button_rect() -> [f32; 4] {
    [6.0, 6.0, 110.0, 20.0]
}

pub fn pause_button_label(language: Language) -> &'static str {
    language.pick(("VexMap: map", "VexMap: карта"))
}

pub struct Ui {
    pub settings: Settings,
    pub page: Page,
    pub view: MapView,
    pub globe: Globe,
    /// The circle of the world the player is in.
    pub world: Disc,
    /// The GPU pass is drawing the ground, so the land and ocean fills would only cover it.
    pub ground_drawn: bool,
    /// The same for the minimap.
    pub minimap_ground_drawn: bool,
    /// 0 by day, 1 by night.
    pub night: f32,
    /// Where globe and flat were looking before "round" took over the view.
    saved_view: Option<([f64; 2], f64)>,
    grab: Option<Grab>,
    rebinding: Option<Row>,
    settings_scroll: f32,
    autotune: Autotune,
    dirty_since: Option<u64>,
    map_ops: Vec<Op>,
    map_stale: bool,
    map_next_ms: u64,
    minimap_ops: Vec<Op>,
    minimap_next_ms: u64,
    labels_taken: HashSet<(i32, i32)>,
    size: [f32; 2],
}

impl Ui {
    pub fn new(settings: Settings, offer_autotune: bool) -> Self {
        Ui {
            settings,
            page: Page::Map,
            view: MapView::default(),
            globe: Globe::default(),
            world: Disc::default(),
            ground_drawn: false,
            minimap_ground_drawn: false,
            night: 0.0,
            saved_view: None,
            grab: None,
            rebinding: None,
            settings_scroll: 0.0,
            autotune: if offer_autotune { Autotune::Offer } else { Autotune::Off },
            dirty_since: None,
            map_ops: Vec::new(),
            map_stale: true,
            map_next_ms: 0,
            minimap_ops: Vec::new(),
            minimap_next_ms: 0,
            labels_taken: HashSet::new(),
            size: [0.0, 0.0],
        }
    }

    /// The map was opened: it starts on the player, at the zoom it was left at.
    pub fn opened(&mut self, player: Option<&Player>) {
        self.page = Page::Map;
        self.grab = None;
        self.rebinding = None;
        self.map_stale = true;
        self.view.disc = self.world;
        if let Some(player) = player {
            self.view.center = [player.x, player.z];
            self.view.clamp_center();
            let world = self.world;
            self.globe.look_at(&world, [player.x, player.z]);
        }
    }

    /// The world's circle changed: the server said so, or the player changed dimension.
    pub fn set_world(&mut self, disc: Disc) {
        if self.world != disc {
            self.world = disc;
            self.view.disc = disc;
            self.view.clamp_center();
            self.map_stale = true;
        }
    }

    /// What the tile cache must cover in this frame.
    pub fn tile_view(&self, size: [f32; 2]) -> TileView {
        let rect = self.map_rect(size);
        if self.settings.map_mode == MapMode::Globe {
            let bpp = self.globe.blocks_per_pixel(&self.world, f64::from(rect[3]));
            let half = bpp * f64::from(rect[2].max(rect[3])) * 0.75;
            return TileView {
                center: self.globe.looking_at(&self.world),
                blocks_per_pixel: bpp,
                half_extent: [half, half],
                disc: self.world,
                whole_world: true,
            };
        }
        let bpp = self.view.blocks_per_pixel;
        TileView {
            center: self.view.center,
            blocks_per_pixel: bpp,
            half_extent: [f64::from(rect[2]) / 2.0 * bpp, f64::from(rect[3]) / 2.0 * bpp],
            disc: self.world,
            whole_world: true,
        }
    }

    pub fn minimap_view(&self, player: &Player) -> TileView {
        let minimap = &self.settings.minimap;
        let zoom = f64::from(minimap.zoom);
        // A turning square shows its corners along any axis.
        let reach = f64::from(minimap.size) / 2.0 * zoom * std::f64::consts::SQRT_2;
        TileView {
            center: [player.x, player.z],
            blocks_per_pixel: zoom,
            half_extent: [reach, reach],
            disc: self.world,
            whole_world: false,
        }
    }

    pub fn minimap_scene(&self, size: [f32; 2], player: &Player, night: f32) -> MapScene {
        let minimap = &self.settings.minimap;
        let rect = self.minimap_rect(size);
        let half = f64::from(rect[2]) / 2.0 * f64::from(minimap.zoom);
        let turn = if minimap.rotate_with_look {
            let (sin, cos) = player.yaw.to_radians().sin_cos();
            [-cos, sin, -sin, -cos]
        } else {
            [1.0, 0.0, 0.0, 1.0]
        };
        MapScene {
            rect: [rect[0] / size[0], rect[1] / size[1], rect[2] / size[0], rect[3] / size[1]],
            mode: if minimap.shape == Shape::Circle { MapMode::Round } else { MapMode::Flat },
            center: [player.x, player.z],
            half_extent: [half, half],
            globe: self.globe,
            disc: self.world,
            night,
            turn,
            opacity: f32::from(minimap.opacity) / 100.0,
            cut_out: true,
        }
    }

    pub fn scene(&self, size: [f32; 2], night: f32) -> MapScene {
        let rect = self.map_rect(size);
        let view = self.tile_view(size);
        MapScene {
            rect: [rect[0] / size[0], rect[1] / size[1], rect[2] / size[0], rect[3] / size[1]],
            mode: self.settings.map_mode,
            center: view.center,
            half_extent: view.half_extent,
            globe: self.globe,
            disc: self.world,
            night,
            turn: [1.0, 0.0, 0.0, 1.0],
            opacity: 1.0,
            cut_out: false,
        }
    }

    /// Where a world point shows on the map, or `None` on the far side of the globe.
    fn to_screen(&self, rect: [f32; 4], world: [f64; 2]) -> Option<[f32; 2]> {
        if self.settings.map_mode != MapMode::Globe {
            return Some(self.view.world_to_screen(rect, world));
        }
        let aspect = f64::from(rect[2] / rect[3].max(1.0));
        let uv = self.globe.project(self.world.to_sphere(world), aspect)?;
        let center = rect_center(rect);
        Some([center[0] + uv[0] as f32 * rect[2] / 2.0, center[1] - uv[1] as f32 * rect[3] / 2.0])
    }

    fn to_world(&self, rect: [f32; 4], screen: [f32; 2]) -> Option<[f64; 2]> {
        if self.settings.map_mode != MapMode::Globe {
            return Some(self.view.screen_to_world(rect, screen));
        }
        let center = rect_center(rect);
        let uv = [
            f64::from((screen[0] - center[0]) / (rect[2] / 2.0)),
            f64::from((center[1] - screen[1]) / (rect[3] / 2.0)),
        ];
        let world = self.world.from_sphere(self.globe.ray(uv, f64::from(rect[2] / rect[3].max(1.0)))?);
        let inside = (world[0] - self.world.center[0]).hypot(world[1] - self.world.center[1]) <= self.world.radius;
        inside.then_some(world)
    }

    pub fn map_ops(&self) -> &[Op] {
        &self.map_ops
    }

    pub fn minimap_ops(&self) -> &[Op] {
        &self.minimap_ops
    }

    fn touched(&mut self, now_ms: u64) {
        self.dirty_since.get_or_insert(now_ms);
        self.map_stale = true;
        self.minimap_next_ms = 0;
    }

    /// `Some` when the settings changed and the player has stopped changing them: the text to write to disk.
    pub fn save_due(&mut self, now_ms: u64) -> Option<String> {
        let since = self.dirty_since?;
        if now_ms.saturating_sub(since) < SAVE_AFTER_MS {
            return None;
        }
        self.dirty_since = None;
        Some(self.settings.to_text())
    }

    /// While the map is up: one frame-rate reading a little over every 100 ms.
    pub fn feed_frame_rate(&mut self, now_ms: u64, fps: u32) {
        let Autotune::Measuring { until_ms, total, samples, next_ms } = &mut self.autotune else { return };
        if now_ms < *next_ms {
            return;
        }
        *next_ms = now_ms + 100;
        *total += u64::from(fps);
        *samples += 1;
        if now_ms < *until_ms {
            return;
        }
        let average = *total as f64 / f64::from((*samples).max(1));
        self.autotune = Autotune::Off;
        self.settings.apply_preset(preset_for_fps(average));
        self.settings.autotuned = true;
        self.touched(now_ms);
    }

    /// The map's area for this GUI size.
    pub fn map_rect(&self, size: [f32; 2]) -> [f32; 4] {
        match self.settings.window_mode {
            WindowMode::Fullscreen => [0.0, 0.0, size[0], size[1]],
            WindowMode::Windowed => {
                let [x, y, w, h] = self.settings.window;
                let w = w.clamp(MIN_WINDOW[0], size[0].max(MIN_WINDOW[0]));
                let h = h.clamp(MIN_WINDOW[1], (size[1] - TITLE_BAR).max(MIN_WINDOW[1]));
                let x = x.clamp(0.0, (size[0] - w).max(0.0));
                let y = y.clamp(TITLE_BAR, (size[1] - h).max(TITLE_BAR));
                [x, y, w, h]
            }
        }
    }

    fn toolbar(&self, rect: [f32; 4]) -> [[f32; 4]; 3] {
        let right = rect[0] + rect[2];
        let top = rect[1] + 4.0;
        [[right - 148.0, top, 90.0, 14.0], [right - 54.0, top, 32.0, 14.0], [right - 18.0, top, 14.0, 14.0]]
    }

    /// The button that switches globe, round and flat.
    fn view_button(&self, rect: [f32; 4]) -> [f32; 4] {
        [rect[0] + rect[2] - 148.0, rect[1] + 22.0, 90.0, 14.0]
    }

    fn autotune_buttons(&self, rect: [f32; 4]) -> [[f32; 4]; 2] {
        let [cx, _] = rect_center(rect);
        let y = rect[1] + 42.0;
        [[cx - 84.0, y, 80.0, 14.0], [cx + 4.0, y, 80.0, 14.0]]
    }

    pub fn map_due(&self, now_ms: u64) -> bool {
        self.map_stale || now_ms >= self.map_next_ms
    }

    pub fn minimap_due(&self, now_ms: u64) -> bool {
        now_ms >= self.minimap_next_ms
    }

    pub fn map_built(&mut self, now_ms: u64) {
        self.map_stale = false;
        self.map_next_ms = now_ms + 1000 / u64::from(self.settings.update_hz.max(1));
    }

    pub fn minimap_built(&mut self, now_ms: u64) {
        self.minimap_next_ms = now_ms + 1000 / u64::from(self.settings.update_hz.max(1));
    }

    pub fn rebuild_map(
        &mut self,
        size: [f32; 2],
        mouse: [f32; 2],
        player: Option<&Player>,
        me: Option<u128>,
        pins: &[Pin<'_>],
        groups: &[Group<'_>],
    ) {
        self.size = size;
        let rect = self.map_rect(size);
        let mut ops = std::mem::take(&mut self.map_ops);
        ops.clear();
        let (view, settings) = (self.view, &self.settings);
        let globe = settings.map_mode == MapMode::Globe;
        if !self.ground_drawn {
            ops.push(Op::Fill { rect, argb: OCEAN });
        }
        let language = settings.language;
        let clip = [Clip::Rect(rect)];
        let center = view.world_to_screen(rect, view.disc.center);
        let radius = (view.disc.radius / view.blocks_per_pixel) as f32;
        // Whole-screen land is one rectangle: cheaper, and clipped wedges leave pinholes where their edges meet.
        let [rx, ry, rw, rh] = rect;
        let circle = Clip::Circle(center, radius);
        if self.ground_drawn {
            // Drawn by the GPU pass.
        } else if [[rx, ry], [rx + rw, ry], [rx, ry + rh], [rx + rw, ry + rh]]
            .iter()
            .all(|corner| circle.contains(*corner))
        {
            ops.push(Op::Fill { rect, argb: LAND });
        } else {
            disc(&mut ops, center, radius, Some(rect), LAND);
        }
        let clips = [Clip::Rect(rect), Clip::Circle(center, radius)];

        let step = grid_step(view.blocks_per_pixel);
        if settings.layers & layer::GRID != 0 && !globe {
            let first = view.screen_to_world(rect, [rect[0], rect[1]]);
            let last = view.screen_to_world(rect, [rect[0] + rect[2], rect[1] + rect[3]]);
            let mut x = (first[0] / step).floor() * step;
            while x <= last[0] {
                let top = view.world_to_screen(rect, [x, first[1]]);
                let bottom = view.world_to_screen(rect, [x, last[1]]);
                line(&mut ops, &clips, top, bottom, 1.0, GRID);
                x += step;
            }
            let mut z = (first[1] / step).floor() * step;
            while z <= last[1] {
                let left = view.world_to_screen(rect, [first[0], z]);
                let right = view.world_to_screen(rect, [last[0], z]);
                line(&mut ops, &clips, left, right, 1.0, GRID);
                z += step;
            }
        }
        if !globe {
            ring(&mut ops, &clip, center, radius, 2.0, RIM);
        }

        let dimension = player.map_or("minecraft:overworld", |player| player.dimension.as_str());
        if settings.layers & layer::CLUSTERS != 0 {
            for group in groups.iter().filter(|group| group.dimension == dimension) {
                let Some(at) = self.to_screen(rect, [group.x, group.z]) else { continue };
                if inside(rect, at) {
                    marker(&mut ops, at, 9.0, GROUP);
                    ops.push(Op::TextCentered { text: group.count.to_string(), at: [at[0], at[1] + 7.0], rgb: WHITE });
                }
            }
        }
        if settings.layers & layer::PLAYERS != 0 {
            let budget = label_budget(settings.labels);
            let mut taken = std::mem::take(&mut self.labels_taken);
            taken.clear();
            let visible = pins.iter().filter(|pin| pin.dimension == dimension && Some(pin.uuid) != me);
            for pin in visible.take(MAX_PINS) {
                let Some(at) = self.to_screen(rect, [pin.x, pin.z]) else { continue };
                if !inside(rect, at) {
                    continue;
                }
                marker(&mut ops, at, 5.0, OTHER_PLAYER);
                // At most one label to a cell, so a crowd stays readable and the batch stays small.
                let cell = ((at[0] / 64.0).floor() as i32, (at[1] / 12.0).floor() as i32);
                if taken.len() < budget && taken.insert(cell) {
                    ops.push(Op::Text { text: pin.name.to_owned(), at: [at[0] + 6.0, at[1] - 4.0], rgb: WHITE });
                }
            }
            self.labels_taken = taken;
        }
        if let Some(player) = player.filter(|player| player.dimension == dimension)
            && let Some(at) = self.to_screen(rect, [player.x, player.z])
            && inside(rect, at)
        {
            arrow(&mut ops, at, yaw_vector(player.yaw), 7.0, SELF_PLAYER);
        }

        let bottom = rect[1] + rect[3] - 12.0;
        if let Some(player) = player {
            ops.push(Op::Text {
                text: format!("X {:.0}  Y {:.0}  Z {:.0}", player.x, player.y, player.z),
                at: [rect[0] + 6.0, bottom],
                rgb: WHITE,
            });
        }
        if !globe {
            ops.push(Op::Text {
                text: format_distance(language, step),
                at: [rect[0] + 6.0, bottom - 11.0],
                rgb: MUTED,
            });
        }
        ops.push(Op::Text {
            text: language.pick(("N", "С")).to_owned(),
            at: [rect[0] + rect[2] / 2.0 - 3.0, rect[1] + 6.0],
            rgb: WHITE,
        });
        if inside(rect, mouse)
            && let Some(world) = self.to_world(rect, mouse)
        {
            ops.push(Op::Text {
                text: format!("{:.0}, {:.0}", world[0], world[1]),
                at: [rect[0] + 6.0, rect[1] + 6.0],
                rgb: MUTED,
            });
        }

        if settings.window_mode == WindowMode::Windowed {
            ops.push(Op::Fill { rect: [rect[0], rect[1] - TITLE_BAR, rect[2], TITLE_BAR], argb: PANEL });
            ops.push(Op::Text { text: "VexMap".to_owned(), at: [rect[0] + 6.0, rect[1] - 12.0], rgb: WHITE });
            ops.push(Op::Fill {
                rect: [rect[0] + rect[2] - GRIP, rect[1] + rect[3] - GRIP, GRIP, GRIP],
                argb: BUTTON_HOVER,
            });
        }
        let [gear, mode, close] = self.toolbar(rect);
        button(&mut ops, gear, language.pick(("Settings", "Настройки")), mouse);
        button(&mut ops, self.view_button(rect), language.pick(settings.map_mode.label()), mouse);
        let mode_label = match settings.window_mode {
            WindowMode::Fullscreen => language.pick(("Win", "Окно")),
            WindowMode::Windowed => language.pick(("Full", "Экран")),
        };
        button(&mut ops, mode, mode_label, mouse);
        button(&mut ops, close, "x", mouse);

        if matches!(self.autotune, Autotune::Offer | Autotune::Measuring { .. }) {
            let [cx, _] = rect_center(rect);
            ops.push(Op::Fill { rect: [cx - 150.0, rect[1] + 22.0, 300.0, 38.0], argb: PANEL });
            if self.autotune == Autotune::Offer {
                ops.push(Op::TextCentered {
                    text: language.pick(("Pick a preset for this PC?", "Подобрать пресет под этот ПК?")).to_owned(),
                    at: [cx, rect[1] + 28.0],
                    rgb: WHITE,
                });
                let [yes, no] = self.autotune_buttons(rect);
                button(&mut ops, yes, language.pick(("Yes", "Да")), mouse);
                button(&mut ops, no, language.pick(("No", "Нет")), mouse);
            } else {
                ops.push(Op::TextCentered {
                    text: language.pick(("Measuring...", "Измеряю...")).to_owned(),
                    at: [cx, rect[1] + 36.0],
                    rgb: WHITE,
                });
            }
        }
        self.map_ops = ops;
    }

    /// Where the minimap is: the box it fills.
    pub fn minimap_rect(&self, size: [f32; 2]) -> [f32; 4] {
        let side = f32::from(self.settings.minimap.size);
        let margin = 6.0;
        let (left, top) = match self.settings.minimap.corner {
            Corner::TopLeft => (true, true),
            Corner::TopRight => (false, true),
            Corner::BottomLeft => (true, false),
            Corner::BottomRight => (false, false),
        };
        [
            if left { margin } else { size[0] - side - margin },
            if top { margin } else { size[1] - side - margin },
            side,
            side,
        ]
    }

    pub fn rebuild_minimap(
        &mut self,
        size: [f32; 2],
        player: &Player,
        me: Option<u128>,
        pins: &[Pin<'_>],
        groups: &[Group<'_>],
    ) {
        let minimap = &self.settings.minimap;
        let rect = self.minimap_rect(size);
        let center = rect_center(rect);
        let half = rect[2] / 2.0;
        let opacity = minimap.opacity;
        let zoom = f64::from(minimap.zoom);
        let land = land_color(self.night);
        let mut ops = std::mem::take(&mut self.minimap_ops);
        ops.clear();
        let clip = match minimap.shape {
            Shape::Circle => {
                if !self.minimap_ground_drawn {
                    disc(&mut ops, center, half, None, with_alpha(land, opacity));
                }
                Clip::Circle(center, half)
            }
            Shape::Square => {
                if !self.minimap_ground_drawn {
                    ops.push(Op::Fill { rect, argb: with_alpha(land, opacity) });
                }
                Clip::Rect(rect)
            }
        };
        // Forward is up when the map turns with the player: see `yaw_vector`, whose result maps to (0, -1).
        let (sin, cos) = if minimap.rotate_with_look {
            let radians = player.yaw.to_radians();
            (radians.sin(), radians.cos())
        } else {
            (0.0, 1.0)
        };
        let rotating = minimap.rotate_with_look;
        let project = |x: f64, z: f64| -> [f32; 2] {
            let (dx, dz) = (((x - player.x) / zoom) as f32, ((z - player.z) / zoom) as f32);
            if rotating {
                [center[0] - dx * cos - dz * sin, center[1] + dx * sin - dz * cos]
            } else {
                [center[0] + dx, center[1] + dz]
            }
        };
        let clips = [clip];
        if minimap.layers & layer::GRID != 0 {
            let step = grid_step(zoom);
            let reach = f64::from(half) * zoom * std::f64::consts::SQRT_2;
            let (x0, x1) = (((player.x - reach) / step).floor() * step, player.x + reach);
            let (z0, z1) = (((player.z - reach) / step).floor() * step, player.z + reach);
            let argb = with_alpha(GRID, opacity);
            let mut x = x0;
            while x <= x1 {
                line(&mut ops, &clips, project(x, z0), project(x, z1), 1.0, argb);
                x += step;
            }
            let mut z = z0;
            while z <= z1 {
                line(&mut ops, &clips, project(x0, z), project(x1, z), 1.0, argb);
                z += step;
            }
        }
        match minimap.shape {
            Shape::Circle => ring(&mut ops, &clips, center, half - 0.5, 1.0, with_alpha(0xFF10_1820, opacity)),
            Shape::Square => {
                let [x, y, w, h] = rect;
                let corners = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
                for index in 0..4 {
                    line(
                        &mut ops,
                        &[],
                        corners[index],
                        corners[(index + 1) % 4],
                        1.0,
                        with_alpha(0xFF10_1820, opacity),
                    );
                }
            }
        }
        if minimap.layers & layer::CLUSTERS != 0 {
            for group in groups.iter().filter(|group| group.dimension == player.dimension) {
                let at = project(group.x, group.z);
                if clip.contains(at) {
                    marker(&mut ops, at, 5.0, GROUP);
                }
            }
        }
        if minimap.layers & layer::PLAYERS != 0 {
            let visible = pins.iter().filter(|pin| pin.dimension == player.dimension && Some(pin.uuid) != me);
            for pin in visible.take(MAX_PINS) {
                let at = project(pin.x, pin.z);
                if clip.contains(at) {
                    marker(&mut ops, at, 3.0, OTHER_PLAYER);
                }
            }
        }
        let heading = if rotating { [0.0, -1.0] } else { yaw_vector(player.yaw) };
        arrow(&mut ops, center, heading, 5.0, SELF_PLAYER);
        let north = project(player.x, player.z - f64::from(half - 8.0) * zoom);
        ops.push(Op::TextCentered {
            text: self.settings.language.pick(("N", "С")).to_owned(),
            at: [north[0], north[1] - 4.0],
            rgb: WHITE,
        });
        self.minimap_ops = ops;
    }

    fn row_rects(&self, size: [f32; 2]) -> Vec<(Row, [f32; 4])> {
        let width = ((size[0] - 24.0) / 2.0).clamp(120.0, 230.0);
        let left = (size[0] / 2.0 - width - 4.0).max(4.0);
        let right = left + width + 8.0;
        let top = 30.0 - self.settings_scroll;
        let column = |rows: &[Row], x: f32| -> Vec<(Row, [f32; 4])> {
            rows.iter()
                .enumerate()
                .map(|(i, row)| (*row, [x, top + i as f32 * ROW_HEIGHT, width, ROW_HEIGHT - 2.0]))
                .collect()
        };
        let mut all = column(&LEFT_ROWS, left);
        all.extend(column(&RIGHT_ROWS, right));
        all
    }

    fn settings_back(&self) -> [f32; 4] {
        [6.0, 6.0, 70.0, 16.0]
    }

    pub fn settings_ops(&self, size: [f32; 2], mouse: [f32; 2], out: &mut Vec<Op>) {
        let language = self.settings.language;
        out.push(Op::Fill { rect: [0.0, 0.0, size[0], size[1]], argb: PANEL });
        out.push(Op::TextCentered {
            text: language.pick(("VexMap settings", "Настройки VexMap")).to_owned(),
            at: [size[0] / 2.0, 11.0],
            rgb: WHITE,
        });
        button(out, self.settings_back(), language.pick(("Back", "Назад")), mouse);
        for (row, rect) in self.row_rects(size) {
            if rect[1] + rect[3] < 26.0 || rect[1] > size[1] {
                continue;
            }
            out.push(Op::Fill { rect, argb: if inside(rect, mouse) { BUTTON_HOVER } else { BUTTON } });
            let y = rect[1] + (rect[3] - 8.0) / 2.0;
            out.push(Op::Text { text: language.pick(row.label()).to_owned(), at: [rect[0] + 4.0, y], rgb: WHITE });
            let value = if self.rebinding == Some(row) {
                language.pick(("press a key", "нажмите клавишу")).to_owned()
            } else {
                row.value(&self.settings)
            };
            // Right-aligned columns need text widths; a fixed value column keeps this list measure-free.
            out.push(Op::Text { text: value, at: [rect[0] + rect[2] * 0.62, y], rgb: MUTED });
        }
    }

    pub fn press(&mut self, size: [f32; 2], mouse: [f32; 2], now_ms: u64) -> Press {
        match self.page {
            Page::Map => self.press_map(size, mouse, now_ms),
            Page::Settings => self.press_settings(size, mouse, now_ms),
        }
    }

    fn press_map(&mut self, size: [f32; 2], mouse: [f32; 2], now_ms: u64) -> Press {
        let rect = self.map_rect(size);
        self.map_stale = true;
        if self.autotune == Autotune::Offer {
            let [yes, no] = self.autotune_buttons(rect);
            if inside(yes, mouse) {
                self.autotune =
                    Autotune::Measuring { until_ms: now_ms + AUTOTUNE_MS, total: 0, samples: 0, next_ms: now_ms };
                return Press::Taken;
            }
            if inside(no, mouse) {
                self.autotune = Autotune::Off;
                self.settings.autotuned = true;
                self.touched(now_ms);
                return Press::Taken;
            }
        }
        let [gear, mode, close] = self.toolbar(rect);
        if inside(close, mouse) {
            return Press::Close;
        }
        if inside(gear, mouse) {
            self.page = Page::Settings;
            return Press::Taken;
        }
        if inside(self.view_button(rect), mouse) {
            self.settings.map_mode = self.settings.map_mode.next();
            if self.settings.map_mode == MapMode::Globe {
                let (world, center) = (self.world, self.view.center);
                self.globe.look_at(&world, center);
            } else if self.settings.map_mode == MapMode::Round {
                // The round map is the whole disc in the window, like a globe flattened. Zooming in works from there.
                let bpp = self.globe.blocks_per_pixel(&self.world, f64::from(rect[3]));
                self.saved_view = Some((self.globe.looking_at(&self.world), bpp));
                self.view.center = self.world.center;
                self.view.blocks_per_pixel = (self.world.radius * 2.0 / f64::from(rect[2].min(rect[3])))
                    .clamp(MIN_BLOCKS_PER_PIXEL, MAX_BLOCKS_PER_PIXEL);
            } else if let Some((center, bpp)) = self.saved_view.take() {
                self.view.center = center;
                self.view.blocks_per_pixel = bpp.clamp(MIN_BLOCKS_PER_PIXEL, MAX_BLOCKS_PER_PIXEL);
                self.view.clamp_center();
            }
            self.touched(now_ms);
            return Press::Taken;
        }
        if inside(mode, mouse) {
            self.settings.window_mode = self.settings.window_mode.next();
            self.touched(now_ms);
            return Press::Taken;
        }
        if self.settings.window_mode == WindowMode::Windowed {
            let grip = [rect[0] + rect[2] - GRIP, rect[1] + rect[3] - GRIP, GRIP, GRIP];
            let title = [rect[0], rect[1] - TITLE_BAR, rect[2], TITLE_BAR];
            if inside(grip, mouse) {
                self.grab = Some(Grab::Resize);
                return Press::Taken;
            }
            if inside(title, mouse) {
                self.grab = Some(Grab::Move);
                return Press::Taken;
            }
        }
        if inside(rect, mouse) {
            self.grab = Some(Grab::Pan);
            return Press::Taken;
        }
        Press::Ignored
    }

    fn press_settings(&mut self, size: [f32; 2], mouse: [f32; 2], now_ms: u64) -> Press {
        if inside(self.settings_back(), mouse) {
            self.page = Page::Map;
            self.rebinding = None;
            self.map_stale = true;
            return Press::Taken;
        }
        let Some((row, _)) = self.row_rects(size).into_iter().find(|(_, rect)| inside(*rect, mouse)) else {
            return Press::Taken;
        };
        if matches!(row, Row::KeyMap | Row::KeyMinimap) {
            self.rebinding = Some(row);
            return Press::Taken;
        }
        row.activate(&mut self.settings);
        self.touched(now_ms);
        Press::Taken
    }

    pub fn release(&mut self) {
        self.grab = None;
    }

    pub fn drag(&mut self, size: [f32; 2], delta: [f32; 2], now_ms: u64) -> bool {
        let Some(grab) = self.grab else { return false };
        match grab {
            Grab::Pan => {
                if self.settings.map_mode == MapMode::Globe {
                    self.globe.drag(delta);
                } else {
                    self.view.pan(delta);
                }
                self.map_stale = true;
            }
            Grab::Move | Grab::Resize => {
                let mut window = self.map_rect(size);
                if grab == Grab::Move {
                    window[0] += delta[0];
                    window[1] += delta[1];
                } else {
                    window[2] += delta[0];
                    window[3] += delta[1];
                }
                self.settings.window = window;
                self.touched(now_ms);
            }
        }
        true
    }

    pub fn scroll(&mut self, size: [f32; 2], mouse: [f32; 2], steps: f64) {
        match self.page {
            Page::Map => {
                let rect = self.map_rect(size);
                if self.settings.map_mode == MapMode::Globe {
                    self.globe.zoom(steps);
                } else {
                    self.view.zoom_at(rect, mouse, steps);
                }
                self.map_stale = true;
            }
            Page::Settings => {
                let rows = LEFT_ROWS.len().max(RIGHT_ROWS.len()) as f32;
                let limit = (30.0 + rows * ROW_HEIGHT + 8.0 - size[1]).max(0.0);
                self.settings_scroll = (self.settings_scroll - steps as f32 * ROW_HEIGHT).clamp(0.0, limit);
            }
        }
    }

    /// A key over the open map. `Taken`: the game must not see it. `Close`: the player pressed the map key again.
    pub fn key(&mut self, key: &str, now_ms: u64) -> Press {
        if let Some(row) = self.rebinding.take() {
            if key != "key.keyboard.escape" {
                match row {
                    Row::KeyMap => self.settings.key_map = key.to_owned(),
                    _ => self.settings.key_minimap = key.to_owned(),
                }
                self.touched(now_ms);
            }
            return Press::Taken;
        }
        if key == self.settings.key_map {
            return Press::Close;
        }
        Press::Ignored
    }

    pub fn toggle_minimap(&mut self, now_ms: u64) {
        self.settings.minimap.enabled = !self.settings.minimap.enabled;
        self.touched(now_ms);
    }
}

// ---------------------------------------------------------------- the round world (ADR-006)
//
// The world is a disc. `Disc::to_sphere` wraps it over a spherical cap by the azimuthal equidistant projection: radius is
// kept, the centre is the smooth pole, and there is no seam. World coordinates stay the truth; only drawing uses this.

/// The polar angle the disc's rim reaches on the sphere: 120 degrees.
pub const THETA_MAX: f64 = 2.094_395_102_393_195_5;
/// Focal length of the globe's camera. The same number as `FOCAL` in the shader.
const FOCAL: f64 = 2.2;
const MIN_GLOBE_DISTANCE: f64 = 1.02;
const MAX_GLOBE_DISTANCE: f64 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Disc {
    pub center: [f64; 2],
    pub radius: f64,
}

impl Default for Disc {
    fn default() -> Self {
        Disc { center: [0.0, 0.0], radius: WORLD_RADIUS }
    }
}

impl Disc {
    /// The unit vector of a world point. +Y is the world's centre.
    pub fn to_sphere(&self, world: [f64; 2]) -> [f64; 3] {
        let (dx, dz) = (world[0] - self.center[0], world[1] - self.center[1]);
        let distance = dx.hypot(dz);
        let theta = distance / self.radius * THETA_MAX;
        let (sin, cos) = theta.sin_cos();
        if distance < 1e-9 {
            return [0.0, 1.0, 0.0];
        }
        [sin * dx / distance, cos, sin * dz / distance]
    }

    /// The world point of a unit vector, past the rim too (that part of the sphere is drawn as ocean).
    pub fn from_sphere(&self, normal: [f64; 3]) -> [f64; 2] {
        let theta = normal[1].clamp(-1.0, 1.0).acos();
        let around = normal[0].hypot(normal[2]);
        if around < 1e-12 {
            return self.center;
        }
        let distance = theta / THETA_MAX * self.radius;
        [self.center[0] + distance * normal[0] / around, self.center[1] + distance * normal[2] / around]
    }
}

/// The orbiting camera of the globe. The camera stays put on -Z and the sphere turns under it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Globe {
    pub yaw: f64,
    pub pitch: f64,
    pub distance: f64,
}

impl Default for Globe {
    fn default() -> Self {
        Globe { yaw: 0.0, pitch: 1.2, distance: 2.6 }
    }
}

impl Globe {
    /// Sphere as the camera sees it, to the sphere's own axes.
    fn turn(&self, hit: [f64; 3]) -> [f64; 3] {
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();
        let v = [hit[0], cp * hit[1] - sp * hit[2], sp * hit[1] + cp * hit[2]];
        [cy * v[0] + sy * v[2], v[1], -sy * v[0] + cy * v[2]]
    }

    fn unturn(&self, normal: [f64; 3]) -> [f64; 3] {
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();
        let v = [cy * normal[0] - sy * normal[2], normal[1], sy * normal[0] + cy * normal[2]];
        [v[0], cp * v[1] + sp * v[2], -sp * v[1] + cp * v[2]]
    }

    /// Points the camera at a world point.
    pub fn look_at(&mut self, disc: &Disc, world: [f64; 2]) {
        let normal = disc.to_sphere(world);
        self.pitch = normal[1].clamp(-1.0, 1.0).asin();
        self.yaw = (-normal[0]).atan2(-normal[2]);
    }

    /// The sphere's point at `uv` (-1..1, +y up), or `None` where the ray misses it.
    pub fn ray(&self, uv: [f64; 2], aspect: f64) -> Option<[f64; 3]> {
        let direction = [uv[0] * aspect, uv[1], FOCAL];
        let length = (direction[0] * direction[0] + direction[1] * direction[1] + direction[2] * direction[2]).sqrt();
        let direction = direction.map(|value| value / length);
        let along = -self.distance * direction[2];
        let square = along * along - (self.distance * self.distance - 1.0);
        if square <= 0.0 {
            return None;
        }
        let travel = -along - square.sqrt();
        Some(self.turn([direction[0] * travel, direction[1] * travel, -self.distance + direction[2] * travel]))
    }

    /// Where a point of the sphere shows, as uv. `None` on the far side: the back of the planet draws nothing.
    pub fn project(&self, normal: [f64; 3], aspect: f64) -> Option<[f64; 2]> {
        let hit = self.unturn(normal);
        if hit[2] >= -1.0 / self.distance {
            return None;
        }
        let depth = hit[2] + self.distance;
        Some([hit[0] / depth * FOCAL / aspect, hit[1] / depth * FOCAL])
    }

    /// Blocks under one GUI pixel at the point the camera looks at.
    fn blocks_per_pixel(&self, disc: &Disc, height: f64) -> f64 {
        disc.radius / THETA_MAX * (self.distance - 1.0) * 2.0 / (FOCAL * height.max(1.0))
    }

    fn looking_at(&self, disc: &Disc) -> [f64; 2] {
        disc.from_sphere(self.turn([0.0, 0.0, -1.0]))
    }

    fn drag(&mut self, delta: [f32; 2]) {
        let speed = 0.005 * (self.distance - 1.0).clamp(0.02, 2.0);
        self.yaw -= f64::from(delta[0]) * speed;
        self.pitch =
            (self.pitch + f64::from(delta[1]) * speed).clamp(-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);
    }

    fn zoom(&mut self, steps: f64) {
        self.distance =
            (1.0 + (self.distance - 1.0) * 0.9_f64.powf(steps)).clamp(MIN_GLOBE_DISTANCE, MAX_GLOBE_DISTANCE);
    }
}

// ---------------------------------------------------------------- theme

/// 0 by day, 1 by night, from the game's clock. Dusk and dawn are smooth ramps, so the map never jumps.
pub fn night_amount(day_time: i64) -> f32 {
    const DUSK: (i64, i64) = (12_000, 13_500);
    const DAWN: (i64, i64) = (22_500, 24_000);
    let time = day_time.rem_euclid(24_000);
    let ramp = |from: i64, to: i64| {
        let t = (time - from) as f32 / (to - from) as f32;
        t * t * (3.0 - 2.0 * t)
    };
    if time < DUSK.0 {
        0.0
    } else if time < DUSK.1 {
        ramp(DUSK.0, DUSK.1)
    } else if time < DAWN.0 {
        1.0
    } else {
        1.0 - ramp(DAWN.0, DAWN.1)
    }
}

/// `vexmap_tiler::Material` numbers to colours. Original, flat, chosen for this map.
const DAY_PALETTE: [u32; 16] = [
    0xE8E4DA, 0x7FB6E6, 0xCFE3B4, 0xA3CC94, 0xF2E5BC, 0xCFCBC2, 0xF7F8F9, 0xCDE7F2, 0xD9C8A9, 0xE0DAD0, 0xFFFFFF,
    0xF08A4B, 0xB57B73, 0xA65E72, 0xE9E5B8, 0xB69CC9,
];
const NIGHT_PALETTE: [u32; 16] = [
    0x20262E, 0x14304F, 0x2E4436, 0x24392E, 0x4A4636, 0x3B4048, 0x6A7480, 0x3F5D70, 0x3D362C, 0x4E5560, 0x79818C,
    0xC85B26, 0x5B3A38, 0x543242, 0x5C5A40, 0x4E4160,
];

/// The minimap's land when no tiles are drawn under it.
fn land_color(night: f32) -> u32 {
    const NIGHT_LAND: u32 = 0xFF2E_4436;
    let mix = |shift: u32| {
        let channel = |color: u32| ((color >> shift) & 0xFF) as f32;
        (channel(LAND) * (1.0 - night) + channel(NIGHT_LAND) * night) as u32
    };
    0xFF00_0000 | (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// The sixteen material colours at this time of day, in linear-free 0..1 RGB as the shader multiplies them.
pub fn palette(night: f32) -> [[f32; 4]; 16] {
    let channel = |color: u32, shift: u32| ((color >> shift) & 0xFF) as f32 / 255.0;
    std::array::from_fn(|index| {
        let (day, dark) = (DAY_PALETTE[index], NIGHT_PALETTE[index]);
        let mix = |shift| channel(day, shift) * (1.0 - night) + channel(dark, shift) * night;
        [mix(16), mix(8), mix(0), 1.0]
    })
}

// ---------------------------------------------------------------- tiles on the client

pub const TILE_SIZE: f64 = 256.0;
/// Layers of the GPU's tile array: 256 is what every wgpu backend guarantees, and at 64 KiB each it is 16 MiB.
pub const TILE_SLOTS: usize = 256;
/// A page table is this many tiles wide.
pub const PAGE_SIZE: usize = 64;
/// Two page tables of RGBA8: the view's level, and the coarse one that always covers the whole world.
pub const PAGE_BYTES: usize = 2 * PAGE_SIZE * PAGE_SIZE * 4;
const MAX_LEVEL: u8 = 12;
const MAX_IN_FLIGHT: usize = 8;
const ANSWER_TIMEOUT_MS: u64 = 2_000;
const RECHECK_MS: u64 = 60_000;
const PLAN_MIN_GAP_MS: u64 = 33;
const PLAN_IDLE_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub level: u8,
    pub x: i32,
    pub z: i32,
}

impl TileKey {
    fn parent(self) -> TileKey {
        TileKey { level: self.level + 1, x: self.x >> 1, z: self.z >> 1 }
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    /// `None`: the server has nothing there.
    slot: Option<u16>,
    version: u32,
    checked_ms: u64,
    used_ms: u64,
}

/// What the view needs of the tile pyramid this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileView {
    pub center: [f64; 2],
    /// Blocks under one GUI pixel.
    pub blocks_per_pixel: f64,
    /// Half the view, in blocks. For a globe, as far as the fine table reaches.
    pub half_extent: [f64; 2],
    pub disc: Disc,
    /// Also keep the coarse table of the whole world. The minimap sees a few hundred blocks and does not need it.
    pub whole_world: bool,
}

impl TileView {
    /// What the page tables depend on: the level and the tiles at the view's corners. The picture moves smoothly in the
    /// shader, so the tables only change when the view crosses a tile.
    fn footprint(&self) -> (u8, [i32; 6], [u64; 3], bool) {
        let level = level_for(self.blocks_per_pixel);
        let span = TILE_SIZE * f64::from(1_u32 << level);
        let cell = |value: f64| (value / span).floor() as i32;
        let [cx, cz] = self.center;
        let [hx, hz] = self.half_extent;
        let cells = [cell(cx), cell(cz), cell(cx - hx), cell(cz - hz), cell(cx + hx), cell(cz + hz)];
        let disc = [self.disc.center[0].to_bits(), self.disc.center[1].to_bits(), self.disc.radius.to_bits()];
        (level, cells, disc, self.whole_world)
    }
}

fn level_for(blocks_per_pixel: f64) -> u8 {
    blocks_per_pixel.max(1.0).log2().floor().clamp(0.0, f64::from(MAX_LEVEL)) as u8
}

/// The coarsest level still fine enough, at which the whole world fits in a page table.
fn coarse_level(disc: &Disc) -> u8 {
    let tiles_wide = 2.0 * disc.radius / TILE_SIZE / (PAGE_SIZE as f64 - 4.0);
    tiles_wide.max(1.0).log2().ceil().clamp(0.0, f64::from(MAX_LEVEL)) as u8
}

/// The client's tiles: which GPU slot each lives in, and what to ask the server next.
pub struct TileCache {
    entries: HashMap<TileKey, Entry>,
    free: Vec<u16>,
    in_flight: HashMap<TileKey, u64>,
    pub pages: Box<[u8; PAGE_BYTES]>,
    /// `[level, origin x, origin z]` of the fine table, then of the coarse one.
    pub tables: [[i32; 3]; 2],
    wants: Vec<(TileKey, u32)>,
    /// Counts the rewrites of `pages`, so the GPU copy happens only after one.
    pub generation: u64,
    planned: Option<(u8, [i32; 6], [u64; 3], bool)>,
    changed: bool,
    next_plan_ms: u64,
}

impl Default for TileCache {
    fn default() -> Self {
        TileCache {
            entries: HashMap::new(),
            free: (0..TILE_SLOTS as u16).rev().collect(),
            in_flight: HashMap::new(),
            pages: Box::new([0; PAGE_BYTES]),
            tables: [[0; 3]; 2],
            wants: Vec::new(),
            generation: 0,
            planned: None,
            changed: true,
            next_plan_ms: 0,
        }
    }
}

impl TileCache {
    /// Another dimension: none of this applies any more. The GPU's slots are simply overwritten.
    pub fn clear(&mut self) {
        let generation = self.generation;
        *self = TileCache { generation, ..TileCache::default() };
    }

    /// The server's answer. Returns the slot to upload `pixels` into, when there are any.
    pub fn answered(&mut self, key: TileKey, version: u32, has_pixels: bool, now_ms: u64) -> Option<u16> {
        self.in_flight.remove(&key);
        self.changed = true;
        if version == 0 {
            if let Some(entry) = self.entries.remove(&key)
                && let Some(slot) = entry.slot
            {
                self.free.push(slot);
            }
            self.entries.insert(key, Entry { slot: None, version: 0, checked_ms: now_ms, used_ms: now_ms });
            return None;
        }
        if !has_pixels {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.checked_ms = now_ms;
            }
            return None;
        }
        let slot = match self.entries.get(&key).and_then(|entry| entry.slot) {
            Some(slot) => slot,
            None => self.take_slot()?,
        };
        self.entries.insert(key, Entry { slot: Some(slot), version, checked_ms: now_ms, used_ms: now_ms });
        Some(slot)
    }

    /// A free slot, or the one used longest ago.
    fn take_slot(&mut self) -> Option<u16> {
        if let Some(slot) = self.free.pop() {
            return Some(slot);
        }
        let oldest = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.slot.is_some())
            .min_by_key(|(_, entry)| entry.used_ms)
            .map(|(key, _)| *key)?;
        self.entries.remove(&oldest).and_then(|entry| entry.slot)
    }

    /// The nearest tile that is on hand, as `[slot low, slot high, levels up, 1]`.
    fn page_entry(&mut self, mut key: TileKey, now_ms: u64) -> [u8; 4] {
        let mut up = 0;
        loop {
            if let Some(entry) = self.entries.get_mut(&key)
                && let Some(slot) = entry.slot
            {
                entry.used_ms = now_ms;
                return [slot as u8, (slot >> 8) as u8, up, 1];
            }
            if key.level >= MAX_LEVEL || up == u8::MAX {
                return [0; 4];
            }
            key = key.parent();
            up += 1;
        }
    }

    fn fill_table(&mut self, table: usize, level: u8, center: [f64; 2], half: [f64; 2], now_ms: u64) {
        let span = TILE_SIZE * f64::from(1_u32 << level);
        let cell = |value: f64| (value / span).floor() as i32;
        let origin = [cell(center[0]) - PAGE_SIZE as i32 / 2, cell(center[1]) - PAGE_SIZE as i32 / 2];
        self.tables[table] = [i32::from(level), origin[0], origin[1]];
        let start = table * PAGE_SIZE * PAGE_SIZE * 4;
        self.pages[start..start + PAGE_SIZE * PAGE_SIZE * 4].fill(0);
        let range = |low: f64, high: f64, origin: i32| {
            ((cell(low) - origin).max(0), (cell(high) - origin).min(PAGE_SIZE as i32 - 1))
        };
        let (left, right) = range(center[0] - half[0], center[0] + half[0], origin[0]);
        let (top, bottom) = range(center[1] - half[1], center[1] + half[1], origin[1]);
        for z in top..=bottom {
            for x in left..=right {
                let key = TileKey { level, x: origin[0] + x, z: origin[1] + z };
                let entry = self.page_entry(key, now_ms);
                let at = start + (z as usize * PAGE_SIZE + x as usize) * 4;
                self.pages[at..at + 4].copy_from_slice(&entry);
                self.want(key, now_ms);
            }
        }
    }

    fn want(&mut self, key: TileKey, now_ms: u64) {
        let due = match self.entries.get(&key) {
            None => true,
            Some(entry) => now_ms.saturating_sub(entry.checked_ms) >= RECHECK_MS,
        };
        if due && !self.in_flight.contains_key(&key) {
            self.wants.push((key, self.entries.get(&key).map_or(0, |entry| entry.version)));
        }
    }

    /// `plan`, when there is a reason to: it costs most of a millisecond on the game's thread (see the bench), so at most
    /// `PLAN_MIN_GAP_MS` apart while the view moves, and `PLAN_IDLE_MS` apart while nothing does.
    pub fn plan_if_due(&mut self, view: &TileView, now_ms: u64) -> Option<Vec<(TileKey, u32)>> {
        let footprint = view.footprint();
        let moved = self.changed || self.planned != Some(footprint);
        let wait = if moved { PLAN_MIN_GAP_MS } else { PLAN_IDLE_MS };
        if now_ms < self.next_plan_ms.saturating_add(wait) && self.planned.is_some() {
            return None;
        }
        self.next_plan_ms = now_ms;
        self.changed = false;
        self.planned = Some(footprint);
        Some(self.plan(view, now_ms))
    }

    /// Rewrites the page tables for this view and returns the tiles to ask for now, nearest first.
    pub fn plan(&mut self, view: &TileView, now_ms: u64) -> Vec<(TileKey, u32)> {
        self.generation += 1;
        self.in_flight.retain(|_, sent| now_ms.saturating_sub(*sent) < ANSWER_TIMEOUT_MS);
        self.wants.clear();
        let coarse = coarse_level(&view.disc);
        let level = level_for(view.blocks_per_pixel).min(coarse);
        let bytes = PAGE_SIZE * PAGE_SIZE * 4;
        if view.whole_world {
            self.fill_table(1, coarse, view.disc.center, [view.disc.radius; 2], now_ms);
        } else {
            self.pages[bytes..2 * bytes].fill(0);
        }
        let coarse_wants = self.wants.len();
        if level < coarse || !view.whole_world {
            self.fill_table(0, level, view.center, view.half_extent, now_ms);
        } else {
            self.tables[0] = self.tables[1];
            self.pages.copy_within(bytes..2 * bytes, 0);
        }
        // Whole-world tiles first: they give the first picture of everything.
        let (first, fine) = self.wants.split_at_mut(coarse_wants);
        let distance = |key: &TileKey| {
            let span = TILE_SIZE * f64::from(1_u32 << key.level);
            ((f64::from(key.x) + 0.5) * span - view.center[0]).hypot((f64::from(key.z) + 0.5) * span - view.center[1])
        };
        let by_distance = |a: &(TileKey, u32), b: &(TileKey, u32)| distance(&a.0).total_cmp(&distance(&b.0));
        first.sort_by(by_distance);
        fine.sort_by(by_distance);
        let room = MAX_IN_FLIGHT.saturating_sub(self.in_flight.len());
        let chosen: Vec<_> = self.wants.iter().take(room).copied().collect();
        for (key, _) in &chosen {
            self.in_flight.insert(*key, now_ms);
        }
        chosen
    }
}

impl TileCache {
    /// Whether a tile is on the GPU now.
    pub fn has_pixels(&self, key: TileKey) -> bool {
        self.entries.get(&key).is_some_and(|entry| entry.slot.is_some())
    }
}

/// `minecraft:overworld` as one safe folder name, `None` for anything else. The reverse of `dimension_from_folder`.
fn dimension_to_folder(dimension: &str) -> Option<String> {
    let safe = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(&b))
            && !part.starts_with('.')
    };
    let (namespace, name) = dimension.split_once(':')?;
    (safe(namespace) && safe(name)).then(|| format!("{namespace}~{name}"))
}

fn dimension_from_folder(folder: &str) -> Option<String> {
    let (namespace, name) = folder.split_once('~')?;
    let dimension = format!("{namespace}:{name}");
    (dimension_to_folder(&dimension).as_deref() == Some(folder)).then_some(dimension)
}

type DiskKey = (String, TileKey);
type SharedVersions = std::sync::Mutex<HashMap<DiskKey, u32>>;

enum DiskJob {
    Store(DiskKey, u32, Vec<u8>),
    Load(DiskKey),
    Limit(u64),
}

/// A tile read back from disk.
pub struct DiskTile {
    pub dimension: String,
    pub key: TileKey,
    pub version: u32,
    pub encoded: Vec<u8>,
}

/// The client's tiles on disk, so a restart or a return to a place costs no network. One worker thread does every file
/// operation: the game's thread only sends jobs and collects answers. Nothing here is the truth, so a file that does not
/// read well is deleted and fetched again. Least recently used tiles go first when the player's limit is reached.
pub struct TileDisk {
    jobs: std::sync::mpsc::Sender<DiskJob>,
    loaded: std::sync::mpsc::Receiver<DiskTile>,
    versions: std::sync::Arc<SharedVersions>,
    limit_bytes: u64,
}

impl TileDisk {
    /// `None` when the thread cannot start: the map then simply works without a disk cache.
    pub fn open(root: std::path::PathBuf, limit_mb: u32) -> Option<TileDisk> {
        let (jobs, job_rx) = std::sync::mpsc::channel();
        let (loaded_tx, loaded) = std::sync::mpsc::channel();
        let versions = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let shared = std::sync::Arc::clone(&versions);
        let limit_bytes = u64::from(limit_mb) << 20;
        std::thread::Builder::new()
            .name("vexmap-tile-disk".to_owned())
            .spawn(move || disk_worker(&root, limit_bytes, &job_rx, &loaded_tx, &shared))
            .ok()?;
        Some(TileDisk { jobs, loaded, versions, limit_bytes })
    }

    /// The version on disk, 0 when there is none (or the scan is not through yet).
    pub fn version(&self, dimension: &str, key: TileKey) -> u32 {
        // `try_lock`: the worker holds it for microseconds, and a miss only costs one needless download.
        self.versions
            .try_lock()
            .ok()
            .and_then(|versions| versions.get(&(dimension.to_owned(), key)).copied())
            .unwrap_or(0)
    }

    pub fn store(&self, dimension: &str, key: TileKey, version: u32, encoded: &[u8]) {
        let _ = self.jobs.send(DiskJob::Store((dimension.to_owned(), key), version, encoded.to_vec()));
    }

    pub fn load(&self, dimension: &str, key: TileKey) {
        let _ = self.jobs.send(DiskJob::Load((dimension.to_owned(), key)));
    }

    pub fn set_limit_mb(&mut self, limit_mb: u32) {
        let bytes = u64::from(limit_mb) << 20;
        if bytes != self.limit_bytes {
            self.limit_bytes = bytes;
            let _ = self.jobs.send(DiskJob::Limit(bytes));
        }
    }

    /// Up to `max` tiles the worker has read since the last call.
    pub fn poll(&self, max: usize) -> Vec<DiskTile> {
        self.loaded.try_iter().take(max).collect()
    }
}

fn tile_path(root: &std::path::Path, (dimension, key): &DiskKey) -> Option<std::path::PathBuf> {
    let folder = dimension_to_folder(dimension)?;
    Some(root.join(folder).join(key.level.to_string()).join(format!("{}.{}.vxt", key.x, key.z)))
}

struct DiskState {
    /// Size and the order of last use, per tile.
    files: HashMap<DiskKey, (u64, u64)>,
    clock: u64,
    total: u64,
}

impl DiskState {
    fn touch(&mut self, key: &DiskKey, size: u64) {
        self.clock += 1;
        let old = self.files.insert(key.clone(), (size, self.clock)).map_or(0, |(old, _)| old);
        self.total = self.total - old + size;
    }
}

fn scan_disk(root: &std::path::Path, state: &mut DiskState) -> HashMap<DiskKey, u32> {
    let mut found = Vec::new();
    let mut versions = HashMap::new();
    let Ok(folders) = std::fs::read_dir(root) else { return versions };
    for folder in folders.flatten() {
        let Some(dimension) = folder.file_name().to_str().and_then(dimension_from_folder) else { continue };
        let Ok(levels) = std::fs::read_dir(folder.path()) else { continue };
        for level in levels.flatten() {
            let Some(level_number) = level.file_name().to_str().and_then(|name| name.parse::<u8>().ok()) else {
                continue;
            };
            let Ok(files) = std::fs::read_dir(level.path()) else { continue };
            for file in files.flatten() {
                let name = file.file_name();
                let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".vxt")) else { continue };
                let Some((x, z)) = stem.split_once('.') else { continue };
                let (Ok(x), Ok(z), Ok(meta)) = (x.parse::<i32>(), z.parse::<i32>(), file.metadata()) else { continue };
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                let key = (dimension.clone(), TileKey { level: level_number, x, z });
                found.push((modified, key, meta.len(), file.path()));
            }
        }
    }
    // Oldest first, so the clock hands out the order of last use.
    found.sort_by_key(|(modified, ..)| *modified);
    for (_, key, size, path) in found {
        let mut head = [0_u8; 4];
        let read = std::fs::File::open(&path).and_then(|mut file| std::io::Read::read_exact(&mut file, &mut head));
        if read.is_err() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        state.touch(&key, size);
        versions.insert(key, u32::from_le_bytes(head));
    }
    versions
}

fn disk_worker(
    root: &std::path::Path,
    mut limit: u64,
    jobs: &std::sync::mpsc::Receiver<DiskJob>,
    loaded: &std::sync::mpsc::Sender<DiskTile>,
    shared: &SharedVersions,
) {
    let mut state = DiskState { files: HashMap::new(), clock: 0, total: 0 };
    let versions = scan_disk(root, &mut state);
    if let Ok(mut shared) = shared.lock() {
        *shared = versions;
    }
    trim_disk(root, &mut state, shared, limit);
    while let Ok(job) = jobs.recv() {
        match job {
            DiskJob::Limit(bytes) => {
                limit = bytes;
                trim_disk(root, &mut state, shared, limit);
            }
            DiskJob::Store(key, version, encoded) => {
                if limit == 0 {
                    continue;
                }
                let Some(path) = tile_path(root, &key) else { continue };
                let mut bytes = Vec::with_capacity(4 + encoded.len());
                bytes.extend_from_slice(&version.to_le_bytes());
                bytes.extend_from_slice(&encoded);
                let temporary = path.with_extension("tmp");
                let written = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&temporary, &bytes))
                    .and_then(|()| std::fs::rename(&temporary, &path));
                if written.is_err() {
                    continue;
                }
                state.touch(&key, bytes.len() as u64);
                if let Ok(mut shared) = shared.lock() {
                    shared.insert(key, version);
                }
                trim_disk(root, &mut state, shared, limit);
            }
            DiskJob::Load(key) => {
                let read = tile_path(root, &key).and_then(|path| std::fs::read(path).ok());
                match read.as_deref().and_then(|bytes| bytes.split_first_chunk::<4>()) {
                    Some((version, encoded)) => {
                        state.touch(&key, 4 + encoded.len() as u64);
                        let tile = DiskTile {
                            dimension: key.0,
                            key: key.1,
                            version: u32::from_le_bytes(*version),
                            encoded: encoded.to_vec(),
                        };
                        if loaded.send(tile).is_err() {
                            return;
                        }
                    }
                    None => forget_tile(root, &mut state, shared, &key),
                }
            }
        }
    }
}

fn forget_tile(root: &std::path::Path, state: &mut DiskState, shared: &SharedVersions, key: &DiskKey) {
    if let Some((size, _)) = state.files.remove(key) {
        state.total -= size;
    }
    if let Some(path) = tile_path(root, key) {
        let _ = std::fs::remove_file(path);
    }
    if let Ok(mut shared) = shared.lock() {
        shared.remove(key);
    }
}

fn trim_disk(root: &std::path::Path, state: &mut DiskState, shared: &SharedVersions, limit: u64) {
    if state.total <= limit {
        return;
    }
    // Down to 90 %, so one new tile does not mean a pass over the list every time.
    let target = limit / 10 * 9;
    let mut order: Vec<_> = state.files.iter().map(|(key, (_, used))| (*used, key.clone())).collect();
    order.sort_unstable_by_key(|(used, _)| *used);
    for (_, key) in order {
        if state.total <= target {
            break;
        }
        forget_tile(root, state, shared, &key);
    }
}

/// What the GPU pass needs for one frame of the map, all in plain numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapScene {
    /// `[x, y, w, h]` as fractions of the frame.
    pub rect: [f32; 4],
    pub mode: MapMode,
    pub center: [f64; 2],
    /// Half the rectangle, in blocks.
    pub half_extent: [f64; 2],
    pub globe: Globe,
    pub disc: Disc,
    pub night: f32,
    /// Screen offset to world offset, `[xx, xy, zx, zy]`, screen y down. The identity unless the minimap turns.
    pub turn: [f32; 4],
    pub opacity: f32,
    /// Leave what is outside the circle untouched (the minimap), instead of painting space (the map).
    pub cut_out: bool,
}

#[cfg(test)]
mod tile_tests {
    use super::*;

    fn close(a: [f64; 2], b: [f64; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6
    }

    #[test]
    fn the_projection_is_reversible_everywhere_on_the_disc() {
        let disc = Disc { center: [120.0, -77.0], radius: 9000.0 };
        for ring in 0..=40 {
            for step in 0..64 {
                let (radius, angle) =
                    (f64::from(ring) / 40.0 * disc.radius, f64::from(step) / 64.0 * std::f64::consts::TAU);
                let world = [disc.center[0] + radius * angle.cos(), disc.center[1] + radius * angle.sin()];
                assert!(close(disc.from_sphere(disc.to_sphere(world)), world), "{world:?}");
            }
        }
    }

    #[test]
    fn the_centre_is_the_pole_and_the_rim_is_at_theta_max() {
        let disc = Disc::default();
        assert_eq!(disc.to_sphere(disc.center), [0.0, 1.0, 0.0]);
        let rim = disc.to_sphere([disc.radius, 0.0]);
        assert!((rim[1] - THETA_MAX.cos()).abs() < 1e-9);
    }

    #[test]
    fn looking_at_a_point_puts_it_in_the_middle_of_the_globe() {
        let disc = Disc::default();
        for world in [[0.0, 0.0], [5000.0, -3000.0], [-9000.0, 1200.0], [100.0, 12000.0]] {
            let mut globe = Globe { distance: 2.0, ..Globe::default() };
            globe.look_at(&disc, world);
            assert!(
                close(
                    globe.looking_at(&disc).map(|v| (v * 1e3).round() / 1e3),
                    world.map(|v: f64| (v * 1e3).round() / 1e3)
                ),
                "{world:?}"
            );
            let uv = globe.project(disc.to_sphere(world), 1.5).unwrap();
            assert!(uv[0].abs() < 1e-6 && uv[1].abs() < 1e-6);
        }
    }

    #[test]
    fn ray_and_project_agree() {
        let disc = Disc::default();
        let mut globe = Globe { distance: 2.5, ..Globe::default() };
        globe.look_at(&disc, [3000.0, 4000.0]);
        for uv in [[0.2, 0.1], [-0.4, 0.3], [0.05, -0.5]] {
            let normal = globe.ray(uv, 1.6).unwrap();
            let back = globe.project(normal, 1.6).unwrap();
            assert!((back[0] - uv[0]).abs() < 1e-9 && (back[1] - uv[1]).abs() < 1e-9);
        }
    }

    #[test]
    fn the_far_side_of_the_planet_does_not_show() {
        let disc = Disc::default();
        let mut globe = Globe::default();
        globe.look_at(&disc, [disc.radius * 0.1, 0.0]);
        assert!(globe.project(disc.to_sphere([-disc.radius, 0.0]), 1.0).is_none());
    }

    #[test]
    fn a_ray_that_misses_the_planet_finds_nothing() {
        assert_eq!(Globe { distance: 2.0, ..Globe::default() }.ray([1.0, 1.0], 2.0), None);
    }

    #[test]
    fn night_comes_and_goes_smoothly() {
        assert_eq!(night_amount(6000), 0.0);
        assert_eq!(night_amount(18000), 1.0);
        let mut last = night_amount(11_900);
        for time in (11_950..13_600).step_by(50) {
            let now = night_amount(time);
            assert!(now >= last && now - last < 0.08, "{time}");
            last = now;
        }
        assert!((night_amount(24_000) - night_amount(0)).abs() < 1e-6);
        assert_eq!(night_amount(6000 + 24000 * 5), 0.0);
    }

    #[test]
    fn the_palette_turns_from_light_to_dark() {
        let brightness = |c: [f32; 4]| c[0] + c[1] + c[2];
        assert!(brightness(palette(0.0)[2]) > brightness(palette(1.0)[2]) * 2.0);
    }

    fn view() -> TileView {
        TileView {
            center: [0.0, 0.0],
            blocks_per_pixel: 1.0,
            half_extent: [300.0, 200.0],
            disc: Disc::default(),
            whole_world: true,
        }
    }

    #[test]
    fn the_world_comes_first_then_what_is_near() {
        let mut cache = TileCache::default();
        let wants = cache.plan(&view(), 0);
        assert_eq!(wants.len(), MAX_IN_FLIGHT);
        assert!(wants.iter().all(|(key, version)| key.level == coarse_level(&view().disc) && *version == 0));
    }

    #[test]
    fn nothing_is_asked_twice_until_the_answer_is_late() {
        let mut cache = TileCache::default();
        let first = cache.plan(&view(), 0);
        let second = cache.plan(&view(), 100);
        assert!(second.iter().all(|want| !first.contains(want)));
        assert!(cache.plan(&view(), ANSWER_TIMEOUT_MS + 1).iter().any(|want| first.contains(want)));
    }

    #[test]
    fn a_missing_tile_falls_back_to_its_parent_and_records_how_far() {
        let mut cache = TileCache::default();
        let parent = TileKey { level: 1, x: 0, z: 0 };
        let slot = cache.answered(parent, 3, true, 0).unwrap();
        let entry = cache.page_entry(TileKey { level: 0, x: 1, z: 1 }, 5);
        assert_eq!(entry, [slot as u8, (slot >> 8) as u8, 1, 1]);
        assert_eq!(cache.page_entry(TileKey { level: 0, x: 900, z: 900 }, 5), [0; 4]);
    }

    #[test]
    fn an_up_to_date_answer_keeps_the_slot_and_nothing_is_reuploaded() {
        let mut cache = TileCache::default();
        let key = TileKey { level: 0, x: 0, z: 0 };
        let slot = cache.answered(key, 3, true, 0);
        assert_eq!(cache.answered(key, 3, false, 10), None);
        assert_eq!(cache.answered(key, 4, true, 20), slot);
    }

    #[test]
    fn the_slot_used_longest_ago_is_the_one_replaced() {
        let mut cache = TileCache::default();
        for x in 0..TILE_SLOTS as i32 {
            cache.answered(TileKey { level: 0, x, z: 0 }, 1, true, x as u64 + 1).unwrap();
        }
        cache.page_entry(TileKey { level: 0, x: 0, z: 0 }, 10_000);
        let reused = cache.answered(TileKey { level: 0, x: 5000, z: 0 }, 1, true, 10_001).unwrap();
        assert!(cache.entries.contains_key(&TileKey { level: 0, x: 0, z: 0 }));
        assert!(!cache.entries.contains_key(&TileKey { level: 0, x: 1, z: 0 }));
        assert_eq!(reused, 1);
    }

    #[test]
    fn levels_follow_the_zoom_and_the_whole_world_fits_the_coarse_table() {
        assert_eq!(level_for(0.25), 0);
        assert_eq!(level_for(1.0), 0);
        assert_eq!(level_for(5.0), 2);
        let disc = Disc { center: [0.0, 0.0], radius: 16_384.0 };
        let span = TILE_SIZE * f64::from(1_u32 << coarse_level(&disc));
        assert!(disc.radius * 2.0 / span <= PAGE_SIZE as f64 - 2.0);
    }

    #[test]
    fn a_tile_at_negative_coordinates_lands_in_the_table() {
        let mut cache = TileCache::default();
        let level = coarse_level(&Disc::default());
        let slot = cache.answered(TileKey { level, x: -1, z: -1 }, 1, true, 0).unwrap();
        cache.plan(&view(), 0);
        let [_, ox, oz] = cache.tables[1];
        let at = PAGE_SIZE * PAGE_SIZE * 4 + (((-1 - oz) as usize) * PAGE_SIZE + (-1 - ox) as usize) * 4;
        assert_eq!(cache.pages[at], slot as u8);
        assert_eq!(cache.pages[at + 3], 1);
    }

    #[test]
    fn the_tables_are_rewritten_only_when_something_changed() {
        let mut cache = TileCache::default();
        assert!(cache.plan_if_due(&view(), 1000).is_some());
        let generation = cache.generation;
        assert!(cache.plan_if_due(&view(), 1010).is_none(), "nothing changed, and it is too soon");
        assert_eq!(cache.generation, generation);
        let moved = TileView { center: [300.0, 0.0], ..view() };
        assert!(cache.plan_if_due(&moved, 1020).is_none(), "the view moved, but 30 times a second is enough");
        assert!(cache.plan_if_due(&moved, 1040).is_some());
        assert!(cache.plan_if_due(&moved, 1050).is_none());
        assert!(cache.plan_if_due(&moved, 1040 + PLAN_IDLE_MS).is_some(), "still asks for tiles when idle");
        cache.answered(TileKey { level: 9, x: 0, z: 0 }, 1, true, 1300);
        assert!(cache.plan_if_due(&moved, 1040 + PLAN_IDLE_MS + PLAN_MIN_GAP_MS).is_some(), "a new tile shows soon");
    }

    #[test]
    fn moving_inside_a_tile_does_not_rewrite_the_tables() {
        let mut cache = TileCache::default();
        assert!(cache.plan_if_due(&view(), 1000).is_some());
        let nudged = TileView { center: [3.0, 2.0], ..view() };
        assert!(cache.plan_if_due(&nudged, 1100).is_none());
    }

    #[test]
    fn the_minimap_never_asks_for_the_whole_world() {
        let mut ui = Ui::new(Settings::default(), false);
        ui.world = Disc { center: [0.0, 0.0], radius: 16_384.0 };
        let player = Player { x: 900.0, y: 64.0, z: 900.0, yaw: 0.0, dimension: "minecraft:overworld".to_owned() };
        let mut cache = TileCache::default();
        let view = ui.minimap_view(&player);
        for (key, _) in cache.plan(&view, 0) {
            assert_eq!(key.level, level_for(view.blocks_per_pixel));
            let span = TILE_SIZE * f64::from(1_u32 << key.level);
            assert!((f64::from(key.x) * span - 900.0).abs() < 1000.0);
        }
    }

    /// `turn` in the scene must be the inverse of the projection the minimap's markers use.
    #[test]
    fn the_turning_minimap_ground_agrees_with_its_markers() {
        let mut settings = Settings::default();
        settings.minimap.rotate_with_look = true;
        let ui = Ui::new(settings, false);
        for yaw in [0.0_f32, 37.0, 90.0, 200.0, -45.0] {
            let player = Player { x: 0.0, y: 64.0, z: 0.0, yaw, dimension: String::new() };
            let [xx, xy, zx, zy] = ui.minimap_scene([640.0, 360.0], &player, 0.0).turn;
            let (sin, cos) = player.yaw.to_radians().sin_cos();
            let (dx, dz) = (12.0_f32, -7.0_f32);
            let (sx, sy) = (-dx * cos - dz * sin, dx * sin - dz * cos);
            assert!((xx * sx + xy * sy - dx).abs() < 1e-4 && (zx * sx + zy * sy - dz).abs() < 1e-4, "{yaw}");
        }
    }

    #[test]
    fn with_tiles_under_it_the_minimap_draws_no_land_of_its_own() {
        let player = Player { x: 0.0, y: 64.0, z: 0.0, yaw: 0.0, dimension: "minecraft:overworld".to_owned() };
        let quads = |drawn: bool| {
            let mut ui = Ui::new(Settings::default(), false);
            ui.minimap_ground_drawn = drawn;
            ui.rebuild_minimap([640.0, 360.0], &player, None, &[], &[]);
            ui.minimap_ops().len()
        };
        assert!(quads(true) < quads(false));
    }

    #[test]
    fn the_round_map_shows_the_whole_world_and_flat_comes_back_to_the_same_place() {
        let mut ui = Ui::new(Settings::default(), false);
        let size = [640.0, 360.0];
        ui.world = Disc { center: [100.0, -50.0], radius: 3600.0 };
        ui.view.disc = ui.world;
        ui.globe.look_at(&ui.world.clone(), [1000.0, 500.0]);
        let rect = ui.map_rect(size);
        let button = ui.view_button(rect);
        let at = [button[0] + 2.0, button[1] + 2.0];
        ui.press(size, at, 0);
        assert_eq!(ui.settings.map_mode, MapMode::Round);
        assert_eq!(ui.view.center, ui.world.center);
        assert!((ui.view.blocks_per_pixel - 20.0).abs() < 1e-9, "the disc exactly fills the window's height");
        ui.press(size, at, 1);
        assert_eq!(ui.settings.map_mode, MapMode::Flat);
        assert!((ui.view.center[0] - 1000.0).abs() < 1e-3 && (ui.view.center[1] - 500.0).abs() < 1e-3);
    }

    #[test]
    fn the_settings_keep_the_map_mode() {
        let mut settings = Settings::default();
        assert_eq!(settings.map_mode, MapMode::Globe);
        settings.map_mode = MapMode::Flat;
        assert_eq!(Settings::parse(&settings.to_text()).map_mode, MapMode::Flat);
    }

    #[test]
    fn the_view_button_cycles_and_markers_show_on_the_globe() {
        let mut ui = Ui::new(Settings::default(), false);
        let size = [640.0, 360.0];
        ui.world = Disc::default();
        ui.opened(Some(&Player { x: 100.0, y: 64.0, z: 100.0, yaw: 0.0, dimension: "minecraft:overworld".to_owned() }));
        let player = Player { x: 100.0, y: 64.0, z: 100.0, yaw: 0.0, dimension: "minecraft:overworld".to_owned() };
        ui.rebuild_map(size, [0.0, 0.0], Some(&player), None, &[], &[]);
        // The player is at the middle of the globe, so the arrow is drawn around the middle of the screen.
        let near_middle = ui.map_ops().iter().any(|op| match op {
            Op::Quad { points, .. } => (points[0][0] - 320.0).abs() < 12.0 && (points[0][1] - 180.0).abs() < 12.0,
            _ => false,
        });
        assert!(near_middle);
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // A test reads best as the default plus what it changes.
mod tests {
    use super::*;

    const SIZE: [f32; 2] = [640.0, 360.0];
    const FULL: [f32; 4] = [0.0, 0.0, 640.0, 360.0];

    fn player() -> Player {
        Player { x: 100.0, y: 64.0, z: -40.0, yaw: 0.0, dimension: "minecraft:overworld".to_owned() }
    }

    fn pin(uuid: u128, x: f64, z: f64) -> Pin<'static> {
        Pin { uuid, name: "Ann", dimension: "minecraft:overworld", x, z }
    }

    #[test]
    fn world_and_screen_round_trip() {
        let view = MapView { center: [123.5, -77.25], blocks_per_pixel: 3.5, disc: Disc::default() };
        let world = view.screen_to_world(FULL, [17.0, 203.0]);
        let back = view.world_to_screen(FULL, world);
        assert!((back[0] - 17.0).abs() < 1e-3 && (back[1] - 203.0).abs() < 1e-3);
    }

    #[test]
    fn north_is_up_and_east_is_right() {
        let view = MapView::default();
        let north = view.world_to_screen(FULL, [0.0, -100.0]);
        let east = view.world_to_screen(FULL, [100.0, 0.0]);
        assert!(north[1] < 180.0);
        assert!(east[0] > 320.0);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let mut view = MapView { center: [50.0, 50.0], blocks_per_pixel: 8.0, disc: Disc::default() };
        let cursor = [500.0, 90.0];
        let before = view.screen_to_world(FULL, cursor);
        view.zoom_at(FULL, cursor, 3.0);
        let after = view.screen_to_world(FULL, cursor);
        assert!((before[0] - after[0]).abs() < 1e-6 && (before[1] - after[1]).abs() < 1e-6);
        assert!(view.blocks_per_pixel < 8.0);
    }

    #[test]
    fn zoom_and_pan_stay_in_bounds() {
        let mut view = MapView::default();
        view.zoom_at(FULL, [0.0, 0.0], 1000.0);
        assert_eq!(view.blocks_per_pixel, MIN_BLOCKS_PER_PIXEL);
        view.zoom_at(FULL, [0.0, 0.0], -1000.0);
        assert_eq!(view.blocks_per_pixel, MAX_BLOCKS_PER_PIXEL);
        view.pan([-1.0e6, 0.0]);
        assert!(view.center[0].hypot(view.center[1]) <= WORLD_RADIUS + 1e-6);
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let mut settings = Settings::default();
        settings.language = Language::En;
        settings.apply_preset(Preset::Roleplay);
        settings.minimap.corner = Corner::BottomLeft;
        settings.minimap.zoom = 0.5;
        settings.window = [1.5, 2.0, 300.0, 200.0];
        settings.key_map = "key.keyboard.j".to_owned();
        settings.autotuned = true;
        assert_eq!(Settings::parse(&settings.to_text()), settings);
    }

    #[test]
    fn a_broken_settings_file_keeps_defaults_and_clamps_values() {
        let settings = Settings::parse(
            "garbage\nlanguage=klingon\nminimap.size=9999\nupdate_hz=0\nminimap.zoom=NaN\nwindow=1,2\n",
        );
        let defaults = Settings::default();
        assert_eq!(settings.language, defaults.language);
        assert_eq!(settings.window, defaults.window);
        assert_eq!(settings.minimap.size, 256);
        assert_eq!(settings.update_hz, 1);
        assert_eq!(settings.minimap.zoom, 2.0);
    }

    #[test]
    fn presets_apply_what_the_spec_asks() {
        let mut settings = Settings::default();
        settings.apply_preset(Preset::Performance);
        assert!(!settings.buildings_3d);
        settings.apply_preset(Preset::Roleplay);
        assert_ne!(settings.layers & layer::DIPLOMACY, 0);
        assert_ne!(settings.layers & layer::FRONT, 0);
        settings.apply_preset(Preset::Gps);
        assert!(settings.minimap.enabled);
        let before = settings.clone();
        settings.apply_preset(Preset::Custom);
        assert_eq!(settings.quality, before.quality);
    }

    #[test]
    fn changing_a_preset_controlled_value_makes_the_preset_custom() {
        let mut settings = Settings::default();
        Row::Labels.activate(&mut settings);
        assert_eq!(settings.preset, Preset::Custom);
        let mut settings = Settings::default();
        Row::MinimapShape.activate(&mut settings);
        assert_eq!(settings.preset, Preset::Balance);
    }

    #[test]
    fn autotune_picks_a_preset_from_the_measured_rate() {
        assert_eq!(preset_for_fps(120.0), Preset::Beauty);
        assert_eq!(preset_for_fps(60.0), Preset::Balance);
        assert_eq!(preset_for_fps(30.0), Preset::Performance);
        let mut ui = Ui::new(Settings::default(), true);
        ui.rebuild_map(SIZE, [0.0, 0.0], None, None, &[], &[]);
        let rect = ui.map_rect(SIZE);
        let [yes, _] = ui.autotune_buttons(rect);
        assert_eq!(ui.press(SIZE, [yes[0] + 1.0, yes[1] + 1.0], 0), Press::Taken);
        for now in (0..=AUTOTUNE_MS + 100).step_by(100) {
            ui.feed_frame_rate(now, 30);
        }
        assert_eq!(ui.settings.preset, Preset::Performance);
        assert!(ui.settings.autotuned);
        assert!(ui.save_due(AUTOTUNE_MS + 1000).is_some());
    }

    #[test]
    fn saving_waits_until_changes_stop_and_writes_once() {
        let mut ui = Ui::new(Settings::default(), false);
        ui.toggle_minimap(1000);
        assert!(ui.save_due(1000 + SAVE_AFTER_MS - 1).is_none());
        assert!(ui.save_due(1000 + SAVE_AFTER_MS).is_some());
        assert!(ui.save_due(5000).is_none());
    }

    #[test]
    fn segments_are_clipped_to_rectangles_and_circles() {
        let (t0, t1) = Clip::Rect([0.0, 0.0, 10.0, 10.0]).segment([-10.0, 5.0], [20.0, 5.0]).unwrap();
        assert!((t0 - 1.0 / 3.0).abs() < 1e-5 && (t1 - 2.0 / 3.0).abs() < 1e-5);
        assert!(Clip::Rect([0.0, 0.0, 10.0, 10.0]).segment([-5.0, -5.0], [-1.0, 20.0]).is_none());
        let (t0, t1) = Clip::Circle([0.0, 0.0], 1.0).segment([-2.0, 0.0], [2.0, 0.0]).unwrap();
        assert!((t0 - 0.25).abs() < 1e-5 && (t1 - 0.75).abs() < 1e-5);
        assert!(Clip::Circle([0.0, 0.0], 1.0).segment([-2.0, 3.0], [2.0, 3.0]).is_none());
    }

    #[test]
    fn a_clipped_triangle_never_leaves_the_rectangle() {
        let rect = [0.0, 0.0, 10.0, 10.0];
        let mut ops = Vec::new();
        clipped_triangle(&mut ops, rect, [[5.0, 5.0], [100.0, 5.0], [5.0, 100.0]], 1);
        assert!(!ops.is_empty());
        for op in &ops {
            let Op::Quad { points, .. } = op else { panic!("{op:?}") };
            for point in points {
                assert!(point[0] >= -1e-3 && point[0] <= 10.001 && point[1] >= -1e-3 && point[1] <= 10.001);
            }
        }
    }

    fn texts(ops: &[Op]) -> Vec<&str> {
        ops.iter()
            .filter_map(|op| match op {
                Op::Text { text, .. } | Op::TextCentered { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_map_skips_other_dimensions_and_ourselves() {
        let mut ui = Ui::new(Settings::default(), false);
        let me = player();
        ui.opened(Some(&me));
        let nether = Pin { dimension: "minecraft:the_nether", name: "Nether", ..pin(2, 100.0, -40.0) };
        let mine = Pin { name: "Me", ..pin(1, 100.0, -40.0) };
        let other = Pin { name: "Other", ..pin(3, 110.0, -40.0) };
        ui.rebuild_map(SIZE, [0.0, 0.0], Some(&me), Some(1), &[nether, mine, other], &[]);
        let shown = texts(ui.map_ops());
        assert!(shown.contains(&"Other"));
        assert!(!shown.contains(&"Nether"));
        assert!(!shown.contains(&"Me"));
    }

    #[test]
    fn a_crowd_is_limited_to_one_label_per_cell_and_the_budget() {
        let mut ui = Ui::new(Settings::default(), false);
        let me = player();
        ui.opened(Some(&me));
        let crowd: Vec<_> = (0..1000).map(|i| pin(i + 2, 100.0 + (i % 40) as f64, -40.0 + (i % 25) as f64)).collect();
        ui.rebuild_map(SIZE, [0.0, 0.0], Some(&me), Some(1), &crowd, &[]);
        let labels = texts(ui.map_ops()).iter().filter(|text| **text == "Ann").count();
        assert!(labels > 0 && labels <= label_budget(ui.settings.labels));
    }

    #[test]
    fn a_view_inside_the_world_is_one_fill_and_the_rim_is_drawn_as_a_disc() {
        let quads = |ui: &Ui| ui.map_ops().iter().filter(|op| matches!(op, Op::Quad { .. })).count();
        let mut ui = Ui::new(Settings::default(), false);
        ui.settings.map_mode = MapMode::Flat;
        ui.settings.layers = 0;
        ui.rebuild_map(SIZE, [0.0, 0.0], None, None, &[], &[]);
        assert_eq!(ui.map_ops()[1], Op::Fill { rect: FULL, argb: LAND });
        ui.view.blocks_per_pixel = MAX_BLOCKS_PER_PIXEL;
        ui.rebuild_map(SIZE, [0.0, 0.0], None, None, &[], &[]);
        assert_ne!(ui.map_ops()[1], Op::Fill { rect: FULL, argb: LAND });
        assert!(quads(&ui) > 32);
    }

    #[test]
    fn hiding_the_players_layer_hides_them() {
        let mut settings = Settings::default();
        settings.layers &= !layer::PLAYERS;
        let mut ui = Ui::new(settings, false);
        let me = player();
        ui.opened(Some(&me));
        ui.rebuild_map(SIZE, [0.0, 0.0], Some(&me), Some(1), &[pin(2, 100.0, -40.0)], &[]);
        assert!(!texts(ui.map_ops()).contains(&"Ann"));
    }

    #[test]
    fn minimap_corners_stay_on_screen() {
        for corner in [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight] {
            let mut settings = Settings::default();
            settings.minimap.corner = corner;
            let ui = Ui::new(settings, false);
            let [x, y, w, h] = ui.minimap_rect(SIZE);
            assert!(x >= 0.0 && y >= 0.0 && x + w <= SIZE[0] && y + h <= SIZE[1], "{corner:?}");
        }
    }

    #[test]
    fn with_the_map_turning_the_player_faces_up() {
        let mut settings = Settings::default();
        settings.minimap.rotate_with_look = true;
        settings.minimap.zoom = 1.0;
        settings.layers = layer::PLAYERS;
        settings.minimap.layers = layer::PLAYERS;
        let mut ui = Ui::new(settings, false);
        let looking = Player { yaw: 90.0, ..player() }; // West.
        // A pin straight ahead, to the west.
        let ahead = pin(2, looking.x - 20.0, looking.z);
        ui.rebuild_minimap(SIZE, &looking, Some(1), &[ahead], &[]);
        let rect = ui.minimap_rect(SIZE);
        let center = rect_center(rect);
        let inner = ui.minimap_ops().iter().find_map(|op| match op {
            Op::Fill { rect, argb } if *argb == OTHER_PLAYER => Some(*rect),
            _ => None,
        });
        let [x, y, w, h] = inner.unwrap();
        assert!((x + w / 2.0 - center[0]).abs() < 0.5);
        assert!(y + h / 2.0 < center[1] - 15.0);
    }

    #[test]
    fn a_pin_outside_the_circular_minimap_is_dropped() {
        let mut settings = Settings::default();
        settings.minimap.zoom = 1.0;
        let mut ui = Ui::new(settings, false);
        let me = player();
        let far = pin(2, me.x + 47.0, me.z + 47.0); // In the square's corner, outside the circle of radius 48.
        ui.rebuild_minimap(SIZE, &me, Some(1), &[far], &[]);
        assert!(!ui.minimap_ops().iter().any(|op| matches!(op, Op::Fill { argb, .. } if *argb == OTHER_PLAYER)));
    }

    fn press_on(ui: &mut Ui, rect: [f32; 4]) -> Press {
        ui.press(SIZE, [rect[0] + 1.0, rect[1] + 1.0], 0)
    }

    #[test]
    fn map_buttons_do_what_they_say() {
        let mut ui = Ui::new(Settings::default(), false);
        ui.rebuild_map(SIZE, [0.0, 0.0], None, None, &[], &[]);
        let [gear, mode, close] = ui.toolbar(FULL);
        assert_eq!(press_on(&mut ui, close), Press::Close);
        assert_eq!(press_on(&mut ui, mode), Press::Taken);
        assert_eq!(ui.settings.window_mode, WindowMode::Windowed);
        ui.settings.window_mode = WindowMode::Fullscreen;
        assert_eq!(press_on(&mut ui, gear), Press::Taken);
        assert_eq!(ui.page, Page::Settings);
    }

    #[test]
    fn dragging_pans_the_map() {
        let mut ui = Ui::new(Settings::default(), false);
        ui.settings.map_mode = MapMode::Flat;
        assert!(!ui.drag(SIZE, [5.0, 0.0], 0));
        assert_eq!(ui.press(SIZE, [200.0, 200.0], 0), Press::Taken);
        let before = ui.view.center;
        assert!(ui.drag(SIZE, [10.0, 0.0], 0));
        assert!(ui.view.center[0] < before[0]);
        ui.release();
        assert!(!ui.drag(SIZE, [10.0, 0.0], 0));
    }

    #[test]
    fn the_window_moves_resizes_and_stays_on_screen() {
        let mut settings = Settings::default();
        settings.window_mode = WindowMode::Windowed;
        settings.window = [50.0, 40.0, 300.0, 200.0];
        let mut ui = Ui::new(settings, false);
        let rect = ui.map_rect(SIZE);
        assert_eq!(press_on(&mut ui, [rect[0] + rect[2] - GRIP, rect[1] + rect[3] - GRIP, GRIP, GRIP]), Press::Taken);
        ui.drag(SIZE, [30.0, 10.0], 0);
        ui.release();
        let grown = ui.map_rect(SIZE);
        assert_eq!((grown[2], grown[3]), (330.0, 210.0));
        press_on(&mut ui, [grown[0], grown[1] - TITLE_BAR, grown[2], TITLE_BAR]);
        ui.drag(SIZE, [10_000.0, 10_000.0], 0);
        let moved = ui.map_rect(SIZE);
        assert!(moved[0] + moved[2] <= SIZE[0] + 0.01 && moved[1] + moved[3] <= SIZE[1] + 0.01);
    }

    #[test]
    fn rebinding_takes_the_next_key_and_escape_cancels() {
        let mut ui = Ui::new(Settings::default(), false);
        ui.page = Page::Settings;
        let rect = ui.row_rects(SIZE).into_iter().find(|(row, _)| *row == Row::KeyMap).unwrap().1;
        assert_eq!(press_on(&mut ui, rect), Press::Taken);
        assert_eq!(ui.key("key.keyboard.j", 0), Press::Taken);
        assert_eq!(ui.settings.key_map, "key.keyboard.j");
        assert_eq!(press_on(&mut ui, rect), Press::Taken);
        assert_eq!(ui.key("key.keyboard.escape", 0), Press::Taken);
        assert_eq!(ui.settings.key_map, "key.keyboard.j");
        assert_eq!(ui.key("key.keyboard.j", 0), Press::Close);
        assert_eq!(ui.key("key.keyboard.k", 0), Press::Ignored);
    }

    #[test]
    fn every_row_is_on_the_settings_page_and_clickable() {
        let mut ui = Ui::new(Settings::default(), false);
        ui.page = Page::Settings;
        let mut ops = Vec::new();
        ui.settings_ops([1280.0, 720.0], [0.0, 0.0], &mut ops);
        for language in [Language::En, Language::Ru] {
            ui.settings.language = language;
            for (row, _) in ui.row_rects(SIZE) {
                assert!(!language.pick(row.label()).is_empty(), "{row:?}");
                let _ = row.value(&ui.settings);
            }
        }
    }

    #[test]
    fn settings_scroll_reaches_every_row_on_a_short_screen() {
        let mut ui = Ui::new(Settings::default(), false);
        ui.page = Page::Settings;
        let short = [640.0, 200.0];
        ui.scroll(short, [0.0, 0.0], -100.0);
        let last = ui.row_rects(short).into_iter().map(|(_, rect)| rect[1] + rect[3]).fold(0.0_f32, f32::max);
        assert!(last <= short[1]);
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vexmap-disk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        let started = Instant::now();
        loop {
            if let Some(found) = probe() {
                return found;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "the disk worker did not answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    const KEY: TileKey = TileKey { level: 3, x: -4, z: 7 };
    const WORLD: &str = "minecraft:overworld";

    #[test]
    fn a_stored_tile_is_known_and_comes_back() {
        let dir = scratch("roundtrip");
        let disk = TileDisk::open(dir.clone(), 1).expect("worker");
        disk.store(WORLD, KEY, 9, &[1, 2, 3]);
        assert_eq!(wait_for(|| Some(disk.version(WORLD, KEY)).filter(|v| *v != 0)), 9);
        disk.load(WORLD, KEY);
        let tile = wait_for(|| disk.poll(1).pop());
        assert_eq!((tile.dimension.as_str(), tile.key, tile.version, tile.encoded), (WORLD, KEY, 9, vec![1, 2, 3]));
        drop(disk);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_session_finds_what_the_last_one_stored() {
        let dir = scratch("restart");
        let first = TileDisk::open(dir.clone(), 1).expect("worker");
        first.store(WORLD, KEY, 4, &[5; 100]);
        wait_for(|| Some(()).filter(|()| first.version(WORLD, KEY) == 4));
        drop(first);
        let second = TileDisk::open(dir.clone(), 1).expect("worker");
        assert_eq!(wait_for(|| Some(second.version(WORLD, KEY)).filter(|v| *v != 0)), 4);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_limit_drops_the_tiles_used_longest_ago() {
        let dir = scratch("limit");
        let disk = TileDisk::open(dir.clone(), 1).expect("worker");
        let payload = vec![7_u8; 100 * 1024];
        let at = |x| TileKey { level: 0, x, z: 0 };
        for x in 0..9 {
            disk.store(WORLD, at(x), 1, &payload);
        }
        wait_for(|| Some(()).filter(|()| disk.version(WORLD, at(8)) == 1));
        // Tile 0 is the oldest, until it is read: then tiles 1 and 2 are.
        disk.load(WORLD, at(0));
        wait_for(|| disk.poll(1).pop());
        disk.store(WORLD, at(90), 1, &payload);
        disk.store(WORLD, at(91), 1, &payload);
        wait_for(|| Some(()).filter(|()| disk.version(WORLD, at(1)) == 0));
        assert_eq!(disk.version(WORLD, at(0)), 1);
        assert_eq!(disk.version(WORLD, at(2)), 0);
        assert_eq!(disk.version(WORLD, at(3)), 1);
        assert_eq!(disk.version(WORLD, at(91)), 1);
        let on_disk: u64 = std::fs::read_dir(dir.join("minecraft~overworld/0"))
            .expect("folder")
            .flatten()
            .map(|file| file.metadata().map_or(0, |meta| meta.len()))
            .sum();
        assert!(on_disk <= 1 << 20, "{on_disk} bytes on disk");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_broken_file_is_forgotten_and_a_hostile_dimension_goes_nowhere() {
        let dir = scratch("broken");
        let disk = TileDisk::open(dir.clone(), 1).expect("worker");
        disk.store(WORLD, KEY, 2, &[1]);
        wait_for(|| Some(()).filter(|()| disk.version(WORLD, KEY) == 2));
        std::fs::write(dir.join("minecraft~overworld/3/-4.7.vxt"), [1, 2]).expect("overwrite");
        disk.load(WORLD, KEY);
        wait_for(|| Some(()).filter(|()| disk.version(WORLD, KEY) == 0));
        disk.store("../evil:x", KEY, 1, &[1]);
        disk.store("minecraft:../x", KEY, 1, &[1]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(disk.version("../evil:x", KEY), 0);
        assert!(!dir.parent().expect("temp").join("evil").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_setting_survives_the_file_and_is_capped() {
        let settings = Settings { tile_cache_mb: 64, ..Settings::default() };
        assert_eq!(Settings::parse(&settings.to_text()).tile_cache_mb, 64);
        assert_eq!(Settings::parse("tile_cache_mb=999999").tile_cache_mb, MAX_TILE_CACHE_MB);
        assert_eq!(Settings::parse("tile_cache_mb=banana").tile_cache_mb, 256);
    }
}
