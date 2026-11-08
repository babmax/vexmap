// VexMap's client interface without the game: settings and presets, the map's view, and what to draw as a list of `Op`s.
// Only `std`, so it is also the `vexmap-ui` crate and is tested and measured without building Horizon. `map_screen.rs`
// replays the lists through Horizon's GUI helpers.
//
// The map is schematic for now: land, grid and players. Tiles, theme and the globe replace the land in Phase 5.

use std::collections::HashSet;

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
    /// `[x, y, w, h]` of the windowed map in GUI pixels.
    pub window: [f32; 4],
    pub key_map: String,
    pub key_minimap: String,
    pub autotuned: bool,
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
            window: [40.0, 30.0, 400.0, 260.0],
            key_map: "key.keyboard.m".to_owned(),
            key_minimap: "key.keyboard.n".to_owned(),
            autotuned: false,
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
            _ => {}
        }
        Some(())
    }

    fn sanitize(&mut self) {
        self.update_hz = self.update_hz.clamp(1, 60);
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
             minimap.rotate_with_look={}\nminimap.opacity={}\nminimap.layers={}\nwindow_mode={}\nwindow={x},{y},{w},{h}\n\
             key_map={}\nkey_minimap={}\nautotuned={}\n",
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
            self.key_map,
            self.key_minimap,
            self.autotuned,
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
}

impl Default for MapView {
    fn default() -> Self {
        MapView { center: [0.0, 0.0], blocks_per_pixel: 2.0 }
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
        let distance = self.center[0].hypot(self.center[1]);
        if distance > WORLD_RADIUS {
            let scale = WORLD_RADIUS / distance;
            self.center = [self.center[0] * scale, self.center[1] * scale];
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
        if let Some(player) = player {
            self.view.center = [player.x, player.z];
            self.view.clamp_center();
        }
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
        ops.push(Op::Fill { rect, argb: OCEAN });
        let (view, settings) = (self.view, &self.settings);
        let language = settings.language;
        let clip = [Clip::Rect(rect)];
        let center = view.world_to_screen(rect, [0.0, 0.0]);
        let radius = (WORLD_RADIUS / view.blocks_per_pixel) as f32;
        // Whole-screen land is one rectangle: cheaper, and clipped wedges leave pinholes where their edges meet.
        let [rx, ry, rw, rh] = rect;
        let circle = Clip::Circle(center, radius);
        if [[rx, ry], [rx + rw, ry], [rx, ry + rh], [rx + rw, ry + rh]].iter().all(|corner| circle.contains(*corner)) {
            ops.push(Op::Fill { rect, argb: LAND });
        } else {
            disc(&mut ops, center, radius, Some(rect), LAND);
        }
        let clips = [Clip::Rect(rect), Clip::Circle(center, radius)];

        let step = grid_step(view.blocks_per_pixel);
        if settings.layers & layer::GRID != 0 {
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
        ring(&mut ops, &clip, center, radius, 2.0, RIM);

        let dimension = player.map_or("minecraft:overworld", |player| player.dimension.as_str());
        if settings.layers & layer::CLUSTERS != 0 {
            for group in groups.iter().filter(|group| group.dimension == dimension) {
                let at = view.world_to_screen(rect, [group.x, group.z]);
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
                let at = view.world_to_screen(rect, [pin.x, pin.z]);
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
        if let Some(player) = player.filter(|player| player.dimension == dimension) {
            let at = view.world_to_screen(rect, [player.x, player.z]);
            if inside(rect, at) {
                arrow(&mut ops, at, yaw_vector(player.yaw), 7.0, SELF_PLAYER);
            }
        }

        let bottom = rect[1] + rect[3] - 12.0;
        if let Some(player) = player {
            ops.push(Op::Text {
                text: format!("X {:.0}  Y {:.0}  Z {:.0}", player.x, player.y, player.z),
                at: [rect[0] + 6.0, bottom],
                rgb: WHITE,
            });
        }
        ops.push(Op::Text { text: format_distance(language, step), at: [rect[0] + 6.0, bottom - 11.0], rgb: MUTED });
        ops.push(Op::Text {
            text: language.pick(("N", "С")).to_owned(),
            at: [rect[0] + rect[2] / 2.0 - 3.0, rect[1] + 6.0],
            rgb: WHITE,
        });
        if inside(rect, mouse) {
            let world = view.screen_to_world(rect, mouse);
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
        let mut ops = std::mem::take(&mut self.minimap_ops);
        ops.clear();
        let clip = match minimap.shape {
            Shape::Circle => {
                disc(&mut ops, center, half, None, with_alpha(LAND, opacity));
                Clip::Circle(center, half)
            }
            Shape::Square => {
                ops.push(Op::Fill { rect, argb: with_alpha(LAND, opacity) });
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
                self.view.pan(delta);
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
                self.view.zoom_at(rect, mouse, steps);
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
        let view = MapView { center: [123.5, -77.25], blocks_per_pixel: 3.5 };
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
        let mut view = MapView { center: [50.0, 50.0], blocks_per_pixel: 8.0 };
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
