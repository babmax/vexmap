// The map screen, the minimap and the pause-menu button, on top of `vexmap_ui`. Decisions live in ui.rs; this file reads
// the game and replays draw lists through Horizon's GUI helpers. Client only: none of these hooks fire on a server.

use super::vexmap as runtime;
use super::vexmap_proto as proto;
use super::vexmap_ui as ui;
use horizon_client::extras::Corner;
use horizon_client::hud::HudBatch;
use horizon_client::mod_api::NativeScreenView;

const MOD_ID: &str = "vexmap";
/// `vexmap_resourcepack::SPRITE_MAP_BUTTON`.
const SPRITE_MAP_BUTTON: &str = "gui/sprites/vexmap/map_button";
const BLOCK_UNITS: f64 = 8.0;

fn is_open(game: &crate::game::Game<'_>) -> bool {
    game.native_mod_screen() == Some(MOD_ID)
}

fn gui_size(batch: &HudBatch) -> [f32; 2] {
    [batch.width, batch.height]
}

fn replay(game: &crate::game::Game<'_>, batch: &mut HudBatch, ops: &[ui::Op]) {
    for op in ops {
        match op {
            ui::Op::Fill { rect, argb } => game.native_fill(batch, *rect, *argb),
            ui::Op::Quad { points, argb } => game.native_quad(batch, *points, *argb),
            ui::Op::Text { text, at, rgb } => game.native_text(batch, text, *at, *rgb, true),
            ui::Op::TextCentered { text, at, rgb } => {
                let width = game.native_text_width(text);
                game.native_text(batch, text, [at[0] - width / 2.0, at[1]], *rgb, true);
            }
            ui::Op::Sprite { name, rect } => {
                if let Some([w, h]) = game.native_sprite_size(name) {
                    game.native_sprite(batch, name, [0.0, 0.0, w, h], *rect);
                }
            }
        }
    }
}

fn local_player(game: &crate::game::Game<'_>) -> Option<ui::Player> {
    game.native_player().map(|player| ui::Player {
        x: player.x,
        y: player.y,
        z: player.z,
        yaw: player.yaw,
        dimension: player.dimension.to_owned(),
    })
}

fn pins(tables: &proto::PositionTables) -> Vec<ui::Pin<'_>> {
    tables
        .markers()
        .map(|marker| ui::Pin {
            uuid: marker.uuid,
            name: &marker.name,
            dimension: &marker.dimension,
            x: f64::from(marker.x) / BLOCK_UNITS,
            z: f64::from(marker.z) / BLOCK_UNITS,
        })
        .collect()
}

fn groups(tables: &proto::PositionTables) -> Vec<ui::Group<'_>> {
    tables
        .clusters()
        .iter()
        .map(|cluster| ui::Group {
            dimension: &cluster.dimension,
            x: f64::from(cluster.x) / BLOCK_UNITS,
            z: f64::from(cluster.z) / BLOCK_UNITS,
            count: cluster.count,
        })
        .collect()
}

/// Opens or closes the screen from a hook that may change the game. Input hooks only get `&Game`, so they ask for it here.
fn apply_requests(game: &mut crate::game::Game<'_>, client: &mut runtime::Client) {
    if std::mem::take(&mut client.close_requested) {
        game.native_mod_screen_close(MOD_ID);
    }
    if std::mem::take(&mut client.open_requested) {
        // TEMPORARY(horizon PR, see docs/phase-1/horizon-prs.md): no helper closes the pause menu, and a mod screen cannot
        // open over it. The same assignment as horizon-ctl's `resume`. The compiler catches a rename when the pin moves.
        if game.screen == crate::game::Screen::Paused {
            game.screen = crate::game::Screen::Playing;
        }
        open(game, client);
    }
}

fn open(game: &mut crate::game::Game<'_>, client: &mut runtime::Client) {
    let player = local_player(game);
    if game.native_mod_screen_open(MOD_ID) {
        client.ui.opened(player.as_ref());
    }
}

/// `/map` from the server.
pub(super) fn open_from_server(game: &mut crate::game::Game<'_>, client: &mut runtime::Client) {
    if game.native_playing() {
        open(game, client);
    }
}

/// A short thread, so the disk never holds a frame. The rename makes the write atomic: a crash leaves the old file.
fn save(path: std::path::PathBuf, text: String) {
    let result = std::thread::Builder::new().name("vexmap-settings".to_owned()).spawn(move || {
        let temporary = path.with_extension("cfg.tmp");
        let written = std::fs::write(&temporary, text).and_then(|()| std::fs::rename(&temporary, &path));
        if let Err(error) = written {
            eprintln!("[vexmap] settings were not saved: {error}");
        }
    });
    if let Err(error) = result {
        eprintln!("[vexmap] settings were not saved: {error}");
    }
}

/// Every frame, on the window's game, from `runtime::frame`.
pub(super) fn frame(game: &mut crate::game::Game<'_>, client: &mut runtime::Client, now_ms: u64) {
    apply_requests(game, client);
    if is_open(game) {
        let (fps, _, _) = game.native_frame_rate();
        client.ui.feed_frame_rate(now_ms, fps);
    }
    if let Some(text) = client.ui.save_due(now_ms)
        && let Some(path) = &client.settings_path
    {
        save(path.clone(), text);
    }
}

pub(super) fn key(game: &mut crate::game::Game<'_>, key: &str, pressed: bool) {
    if !pressed || !game.native_playing() {
        return;
    }
    let now = runtime::now_ms();
    runtime::with_client(|client| {
        if key == client.ui.settings.key_map {
            open(game, client);
        } else if key == client.ui.settings.key_minimap {
            client.ui.toggle_minimap(now);
        }
    });
}

pub(super) fn screen_render_post(game: &crate::game::Game<'_>, batch: &mut HudBatch, screen: &NativeScreenView<'_>) {
    let now = runtime::now_ms();
    if screen.id == "paused" {
        draw_pause_button(game, batch, screen);
    } else if is_open(game) {
        runtime::with_client(|client| draw_map(game, batch, screen.mouse, client, now));
    }
}

fn draw_pause_button(game: &crate::game::Game<'_>, batch: &mut HudBatch, screen: &NativeScreenView<'_>) {
    let Some(language) = runtime::with_client(|client| client.ui.settings.language) else { return };
    let rect = ui::pause_button_rect();
    let [x, y, w, h] = rect;
    let hover = screen.mouse[0] >= x && screen.mouse[0] <= x + w && screen.mouse[1] >= y && screen.mouse[1] <= y + h;
    let ops = [
        ui::Op::Fill { rect, argb: if hover { 0xFF3C_5068 } else { 0xE010_1820 } },
        ui::Op::Sprite { name: SPRITE_MAP_BUTTON, rect: [x + 2.0, y + 2.0, 16.0, 16.0] },
        ui::Op::Text { text: ui::pause_button_label(language).to_owned(), at: [x + 22.0, y + 6.0], rgb: 0xFF_FFFF },
    ];
    replay(game, batch, &ops);
}

fn draw_map(
    game: &crate::game::Game<'_>,
    batch: &mut HudBatch,
    mouse: [f32; 2],
    client: &mut runtime::Client,
    now: u64,
) {
    let size = gui_size(batch);
    client.gui_size = size;
    match client.ui.page {
        ui::Page::Map => {
            if client.ui.map_due(now) {
                let player = local_player(game);
                let (shown, clusters) = (pins(&client.positions), groups(&client.positions));
                client.ui.rebuild_map(size, mouse, player.as_ref(), client.me, &shown, &clusters);
                client.ui.map_built(now);
            }
            replay(game, batch, client.ui.map_ops());
        }
        ui::Page::Settings => {
            client.scratch.clear();
            client.ui.settings_ops(size, mouse, &mut client.scratch);
            replay(game, batch, &client.scratch);
        }
    }
}

/// The HUD, drawn each frame unless F1 hides it. The list is rebuilt at the preset's rate and replayed in between.
pub(super) fn hud(game: &crate::game::Game<'_>, batch: &mut HudBatch) {
    if game.native_mod_screen().is_some() {
        return;
    }
    let now = runtime::now_ms();
    let size = gui_size(batch);
    runtime::with_client(|client| {
        if !client.ui.settings.minimap.enabled
            || !game.native_corner_shown(corner_of(client.ui.settings.minimap.corner))
        {
            return;
        }
        if client.ui.minimap_due(now) {
            let Some(player) = local_player(game) else { return };
            let (shown, clusters) = (pins(&client.positions), groups(&client.positions));
            client.ui.rebuild_minimap(size, &player, client.me, &shown, &clusters);
            client.ui.minimap_built(now);
        }
        replay(game, batch, client.ui.minimap_ops());
    });
}

fn corner_of(corner: ui::Corner) -> Corner {
    match corner {
        ui::Corner::TopLeft => Corner::TopLeft,
        ui::Corner::TopRight => Corner::TopRight,
        ui::Corner::BottomLeft => Corner::BottomLeft,
        ui::Corner::BottomRight => Corner::BottomRight,
    }
}

/// While the full-screen map is up the HUD under it would only shine through.
pub(super) fn gui_layer_pre(game: &crate::game::Game<'_>, canceled: &mut bool) {
    if is_open(game)
        && runtime::with_client(|client| client.ui.settings.window_mode == ui::WindowMode::Fullscreen).unwrap_or(false)
    {
        *canceled = true;
    }
}

pub(super) fn mouse_pressed(
    game: &crate::game::Game<'_>,
    screen: &NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    if button != 0 {
        return;
    }
    if screen.id == "paused" {
        let [x, y, w, h] = ui::pause_button_rect();
        let mouse = screen.mouse;
        if mouse[0] >= x && mouse[0] <= x + w && mouse[1] >= y && mouse[1] <= y + h {
            runtime::with_client(|client| client.open_requested = true);
            *canceled = true;
        }
        return;
    }
    if !is_open(game) {
        return;
    }
    let now = runtime::now_ms();
    runtime::with_client(|client| match client.ui.press(client.gui_size, screen.mouse, now) {
        ui::Press::Ignored => {}
        ui::Press::Taken => *canceled = true,
        ui::Press::Close => {
            client.close_requested = true;
            *canceled = true;
        }
    });
}

pub(super) fn mouse_released(game: &crate::game::Game<'_>, button: i32) {
    if button == 0 && is_open(game) {
        runtime::with_client(|client| client.ui.release());
    }
}

pub(super) fn mouse_dragged(game: &crate::game::Game<'_>, delta: [f32; 2], canceled: &mut bool) {
    if !is_open(game) {
        return;
    }
    let now = runtime::now_ms();
    if runtime::with_client(|client| client.ui.drag(client.gui_size, delta, now)).unwrap_or(false) {
        *canceled = true;
    }
}

pub(super) fn mouse_scrolled(
    game: &crate::game::Game<'_>,
    screen: &NativeScreenView<'_>,
    scroll: [f64; 2],
    canceled: &mut bool,
) {
    if !is_open(game) {
        return;
    }
    runtime::with_client(|client| client.ui.scroll(client.gui_size, screen.mouse, scroll[1]));
    *canceled = true;
}

pub(super) fn key_pressed(game: &crate::game::Game<'_>, key: &str, repeat: bool, canceled: &mut bool) {
    if repeat || !is_open(game) {
        return;
    }
    let now = runtime::now_ms();
    runtime::with_client(|client| match client.ui.key(key, now) {
        ui::Press::Ignored => {}
        ui::Press::Taken => *canceled = true,
        ui::Press::Close => {
            client.close_requested = true;
            *canceled = true;
        }
    });
}
