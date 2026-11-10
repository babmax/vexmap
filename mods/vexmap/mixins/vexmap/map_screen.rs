// The map screen, the minimap and the pause-menu button, on top of `vexmap_ui`. Decisions live in ui.rs; this file reads
// the game and replays draw lists through Horizon's GUI helpers. Client only: none of these hooks fire on a server.

use super::vexmap as runtime;
use super::vexmap_editor as editor;
use super::vexmap_proto as proto;
use super::vexmap_routing as routing;
use super::vexmap_ui as ui;
use horizon_client::extras::Corner;
use horizon_client::hud::HudBatch;
use horizon_client::mod_api::NativeScreenView;

const MOD_ID: &str = "vexmap";
/// `vexmap_resourcepack::SPRITE_MAP_BUTTON`.
const SPRITE_MAP_BUTTON: &str = "gui/sprites/vexmap/map_button";
const BLOCK_UNITS: f64 = 8.0;
/// The route network has no dimension of its own yet: it is drawn on the overworld.
const NETWORK_DIMENSION: &str = "minecraft:overworld";
/// What one rebuild of the map may draw. A crowded view loses its shortest roads first.
const MAX_DRAWN_LINES: usize = 6000;
const MAX_DRAWN_NODES: usize = 2000;
/// Zoomed out past this, single nodes are a blur.
const MAX_NODE_BLOCKS_PER_PIXEL: f64 = 8.0;
const _: () = assert!(editor::MAX_ROUTE_STOPS + 1 == routing::MAX_STOPS);

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

fn to_key(id: proto::TileId) -> ui::TileKey {
    ui::TileKey { level: id.level, x: id.x, z: id.z }
}

/// A tile from the server. Anything malformed is dropped, and the map keeps what it has.
pub(super) fn tile_arrived(
    client: &mut runtime::Client,
    dimension: &str,
    id: proto::TileId,
    version: u32,
    encoded: &[u8],
    now_ms: u64,
) {
    if dimension != client.tiles_dimension {
        return;
    }
    let mut pixels = Box::new([0_u8; proto::TILE_AREA]);
    if !encoded.is_empty() && proto::decode_tile(encoded, &mut pixels).is_err() {
        return;
    }
    if !encoded.is_empty() && dimension == NETWORK_DIMENSION {
        client.terrain.store(id.level, id.x, id.z, &pixels[..]);
    }
    let key = to_key(id);
    if let Some(slot) = client.tiles.answered(key, version, !encoded.is_empty(), now_ms) {
        horizon_client_render::gfx::vexmap_render::queue_tile(slot, pixels);
        if let Some(disk) = &client.tile_disk {
            disk.store(dimension, key, version, encoded);
        }
    } else if encoded.is_empty() && version != 0 && !client.tiles.has_pixels(key) {
        // "Unchanged" for a tile we asked about by its disk version: the pixels are on disk.
        if let Some(disk) = &client.tile_disk {
            disk.load(dimension, key);
        }
    }
}

/// Tiles the disk thread has read, handled like ones from the server, a few per frame.
fn disk_tiles(client: &mut runtime::Client, now_ms: u64) {
    let Some(disk) = &client.tile_disk else { return };
    for tile in disk.poll(8) {
        if tile.dimension != client.tiles_dimension {
            continue;
        }
        let mut pixels = Box::new([0_u8; proto::TILE_AREA]);
        if proto::decode_tile(&tile.encoded, &mut pixels).is_err() {
            continue;
        }
        if tile.dimension == NETWORK_DIMENSION {
            client.terrain.store(tile.key.level, tile.key.x, tile.key.z, &pixels[..]);
        }
        if let Some(slot) = client.tiles.answered(tile.key, tile.version, true, now_ms) {
            horizon_client_render::gfx::vexmap_render::queue_tile(slot, pixels);
        }
    }
}

fn ask_for_tiles(game: &mut crate::game::Game<'_>, client: &mut runtime::Client, wants: &[(ui::TileKey, u32)]) {
    if wants.is_empty() {
        return;
    }
    let disk = client.tile_disk.as_ref();
    let dimension = &client.tiles_dimension;
    let wants = wants
        .iter()
        .map(|(key, version)| proto::TileWant {
            id: proto::TileId { level: key.level, x: key.x, z: key.z },
            // A tile we know nothing about may still be on disk: then the server only has to say "unchanged".
            version: if *version == 0 { disk.map_or(0, |disk| disk.version(dimension, *key)) } else { *version },
        })
        .collect();
    let message = proto::Message::TileWants { dimension: client.tiles_dimension.clone(), wants };
    runtime::send_message(game, client, &message);
}

/// The disk cache sits next to the settings file. Tried once; the player's limit is followed while it runs.
fn open_tile_disk(client: &mut runtime::Client) {
    let limit = client.ui.settings.tile_cache_mb;
    if let Some(disk) = &mut client.tile_disk {
        disk.set_limit_mb(limit);
    } else if !client.tile_disk_tried {
        client.tile_disk_tried = true;
        if let Some(root) = client.settings_path.as_ref().and_then(|path| path.parent()).map(|dir| dir.join("tiles")) {
            client.tile_disk = ui::TileDisk::open(root, limit);
        }
    }
}

/// Keeps the tiles, the page tables and the GPU pass in step with the view. Once per frame while the map is up.
fn update_ground(game: &mut crate::game::Game<'_>, client: &mut runtime::Client, now_ms: u64) {
    use horizon_client_render::gfx::vexmap_render as render;
    let open = is_open(game);
    let minimap_up = !open
        && game.native_mod_screen().is_none()
        && client.ui.settings.minimap.enabled
        && game.native_corner_shown(corner_of(client.ui.settings.minimap.corner));
    let map_up = open && client.ui.page == ui::Page::Map;
    let player = local_player(game);
    client.ui.ground_drawn = false;
    client.ui.minimap_ground_drawn = false;
    let (Some(player), true, true) = (player, map_up || minimap_up, client.gui_size[0] > 0.0) else {
        render::set_scene(None);
        return;
    };
    if client.tiles_dimension != player.dimension {
        client.tiles.clear();
        client.tiles_dimension.clone_from(&player.dimension);
    }
    let epoch = render::gpu_epoch();
    if client.gpu_epoch != epoch {
        // A new device has none of the tiles the cache remembers.
        client.gpu_epoch = epoch;
        client.tiles.clear();
    }
    if let Some(disc) = client.discs.get(&player.dimension).copied() {
        client.ui.set_world(disc);
    }
    open_tile_disk(client);
    disk_tiles(client, now_ms);
    let night = ui::night_amount(game.native_day_time());
    client.ui.night = night;
    let view = if map_up { client.ui.tile_view(client.gui_size) } else { client.ui.minimap_view(&player) };
    if let Some(wants) = client.tiles.plan_if_due(&view, now_ms) {
        ask_for_tiles(game, client, &wants);
    }
    let scene = if map_up {
        client.ui.scene(client.gui_size, night)
    } else {
        client.ui.minimap_scene(client.gui_size, &player, night)
    };
    let pages_changed = client.sent_generation != client.tiles.generation;
    client.sent_generation = client.tiles.generation;
    render::set_scene(Some(render_scene(&scene, &client.tiles, pages_changed)));
    client.ui.ground_drawn = map_up;
    client.ui.minimap_ground_drawn = minimap_up;
}

fn render_scene(
    scene: &ui::MapScene,
    tiles: &ui::TileCache,
    pages_changed: bool,
) -> horizon_client_render::gfx::vexmap_render::Scene {
    let [fine, coarse] = tiles.tables;
    horizon_client_render::gfx::vexmap_render::Scene {
        rect: scene.rect,
        mode: match scene.mode {
            ui::MapMode::Globe => 0,
            ui::MapMode::Round => 1,
            ui::MapMode::Flat => 2,
        },
        center: scene.center.map(|value| value as f32),
        half_extent: scene.half_extent.map(|value| value as f32),
        camera: [scene.globe.yaw as f32, scene.globe.pitch as f32, scene.globe.distance as f32],
        theta_max: ui::THETA_MAX as f32,
        disc: [scene.disc.center[0] as f32, scene.disc.center[1] as f32, scene.disc.radius as f32],
        night: scene.night,
        fine,
        coarse,
        palette: ui::palette(scene.night),
        turn: scene.turn,
        opacity: scene.opacity,
        cut_out: scene.cut_out,
        pages: pages_changed.then(|| tiles.pages.clone()),
    }
}

fn mode_from_wire(mode: proto::LineMode) -> editor::Mode {
    match mode {
        proto::LineMode::Walk => editor::Mode::Walk,
        proto::LineMode::Boat => editor::Mode::Boat,
        proto::LineMode::Train => editor::Mode::Train,
    }
}

fn mode_to_wire(mode: editor::Mode) -> proto::LineMode {
    match mode {
        editor::Mode::Walk => proto::LineMode::Walk,
        editor::Mode::Boat => proto::LineMode::Boat,
        editor::Mode::Train => proto::LineMode::Train,
    }
}

fn edit_to_wire(edit: editor::Edit) -> proto::NetworkEdit {
    match edit {
        editor::Edit::AddNode { x, z, transfer } => proto::NetworkEdit::AddNode { x, z, transfer },
        editor::Edit::AddLine { a, b, mode, one_way } => {
            proto::NetworkEdit::AddLine { a, b, mode: mode_to_wire(mode), one_way }
        }
        editor::Edit::SetClosed { line, closed } => proto::NetworkEdit::SetClosed { line, closed },
        editor::Edit::Revert { seq } => proto::NetworkEdit::Revert { seq },
    }
}

/// A page of the route network from the server.
pub(super) fn network_page(client: &mut runtime::Client, page: &proto::NetworkPage, now_ms: u64) {
    let nodes = page.nodes.iter().map(|n| editor::Node { id: n.id, x: n.x, z: n.z, transfer: n.transfer }).collect();
    let lines = page
        .lines
        .iter()
        .map(|l| editor::Line {
            id: l.id,
            a: l.a,
            b: l.b,
            mode: mode_from_wire(l.mode),
            one_way: l.one_way,
            closed: l.closed,
        })
        .collect();
    let page = editor::Page {
        since: page.since,
        to: page.to,
        latest: page.latest,
        may_edit: page.may_edit,
        render_done: page.render_done,
        render_total: page.render_total,
        rendering: page.rendering,
        nodes,
        lines,
        removed_nodes: page.removed_nodes.clone(),
        removed_lines: page.removed_lines.clone(),
    };
    client.workspace.page(&page, now_ms);
}

/// A page of the network's history from the server.
pub(super) fn network_history(client: &mut runtime::Client, history: &proto::NetworkHistory) {
    let rows = history
        .entries
        .iter()
        .map(|entry| editor::HistoryRow {
            seq: entry.seq,
            age_seconds: entry.age_seconds,
            actor: entry.actor.clone(),
            kind: match entry.kind {
                proto::HistoryKind::NodeAdded => editor::HistoryKind::NodeAdded,
                proto::HistoryKind::LineAdded => editor::HistoryKind::LineAdded,
                proto::HistoryKind::LineClosed => editor::HistoryKind::LineClosed,
                proto::HistoryKind::LineOpened => editor::HistoryKind::LineOpened,
                proto::HistoryKind::NodeRemoved => editor::HistoryKind::NodeRemoved,
                proto::HistoryKind::LineRemoved => editor::HistoryKind::LineRemoved,
            },
            place: entry.place,
            undone: entry.undone,
        })
        .collect();
    client.workspace.history_page(history.before, history.more, rows);
}

/// What became of one of our edits.
pub(super) fn edit_result(client: &mut runtime::Client, token: u32, status: proto::EditStatus, id: u64, now_ms: u64) {
    let status = match status {
        proto::EditStatus::Done => editor::EditStatus::Done,
        proto::EditStatus::AlreadyDone => editor::EditStatus::AlreadyDone,
        proto::EditStatus::Forbidden => editor::EditStatus::Forbidden,
        proto::EditStatus::Invalid => editor::EditStatus::Invalid,
        proto::EditStatus::Unavailable => editor::EditStatus::Unavailable,
    };
    client.workspace.result(token, status, id, now_ms);
}

/// Asks for what is new in the network and sends the edits the input hooks left. Once per frame.
fn sync_network(game: &mut crate::game::Game<'_>, client: &mut runtime::Client, now_ms: u64) {
    if let Some(since) = client.workspace.want_due(now_ms, is_open(game)) {
        let render = client.workspace.take_want_render();
        runtime::send_message(game, client, &proto::Message::NetworkWant { since, render });
    }
    if let Some(before) = client.workspace.history_due(now_ms) {
        runtime::send_message(game, client, &proto::Message::NetworkHistoryWant { before });
    }
    for (token, edit) in client.workspace.take_outbox() {
        let message = proto::Message::NetworkEdit { token, edit: edit_to_wire(edit) };
        runtime::send_message(game, client, &message);
    }
}

/// A search under way on its own thread: a large network must not cost a frame.
pub(super) struct RouteSearch {
    stops: Vec<routing::Point>,
    answer: std::sync::mpsc::Receiver<Result<routing::Route, routing::RouteError>>,
}

fn routing_mode(mode: editor::Mode) -> routing::Mode {
    match mode {
        editor::Mode::Walk => routing::Mode::Walk,
        editor::Mode::Boat => routing::Mode::Boat,
        editor::Mode::Train => routing::Mode::Train,
    }
}

/// Builds a route from `from` through `stops`, in order.
fn start_search(client: &mut runtime::Client, from: routing::Point, stops: Vec<routing::Point>) {
    let nodes = client.workspace.network.nodes().to_vec();
    let lines = client.workspace.network.lines().to_vec();
    let speeds = client.navigator.speeds();
    let terrain = client.terrain.clone();
    let mut through = Vec::with_capacity(stops.len() + 1);
    through.push(from);
    through.extend_from_slice(&stops);
    let (sender, answer) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("vexmap-route".to_owned()).spawn(move || {
        let mut graph = routing::Graph::new();
        let mut ids = std::collections::HashMap::with_capacity(nodes.len());
        for node in &nodes {
            ids.insert(node.id, graph.add_node(f64::from(node.x), f64::from(node.z), node.transfer));
        }
        for line in &lines {
            let (Some(a), Some(b)) = (ids.get(&line.a), ids.get(&line.b)) else { continue };
            let Ok(edges) = graph.add_line(*a, *b, routing_mode(line.mode), line.one_way) else { continue };
            if line.closed {
                for edge in edges {
                    graph.set_closed(edge, true);
                }
            }
        }
        let modes = [routing::Mode::Walk, routing::Mode::Boat, routing::Mode::Train];
        let _ = sender.send(graph.route_over(&through, &modes, &speeds, &terrain));
    });
    match spawned {
        Ok(_) => client.route_search = Some(RouteSearch { stops, answer }),
        Err(error) => {
            eprintln!("[vexmap] route search did not start: {error}");
            show_route(client);
        }
    }
}

/// Copies the route into the shapes the map draws.
fn show_route(client: &mut runtime::Client) {
    let mut steps = Vec::new();
    if let Some(route) = client.navigator.route() {
        for leg in &route.legs {
            for pair in leg.points.windows(2) {
                steps.push(ui::RouteStep {
                    a: [pair[0].x, pair[0].z],
                    b: [pair[1].x, pair[1].z],
                    direct: leg.kind == routing::LegKind::Direct,
                });
            }
        }
    }
    client.ui.route = steps;
    let stops = client.navigator.stops();
    client.ui.stops = stops[..stops.len().saturating_sub(1)].iter().map(|stop| [stop.x, stop.z]).collect();
    client.ui.destination = client.navigator.destination().map(|end| [end.x, end.z]);
    client.workspace.route_stops = stops.iter().map(|stop| (stop.x, stop.z)).collect();
    client.workspace.route_shown = client.navigator.route().is_some();
    client.workspace.off_route = false;
    if client.navigator.route().is_none() {
        client.ui.banner = None;
    }
}

fn collect_search(client: &mut runtime::Client, now_ms: u64) {
    let Some(search) = &client.route_search else { return };
    let Ok(answer) = search.answer.try_recv() else { return };
    let stops = search.stops.clone();
    client.route_search = None;
    match answer {
        Ok(route) => {
            if client.diagnostics {
                eprintln!("[vexmap] route: legs={} length={:.0}", route.legs.len(), route.length);
            }
            client.navigator.set_route(stops, route);
        }
        Err(_) => client.workspace.tell(editor::Notice::NoRoute, now_ms),
    }
    show_route(client);
}

/// Everything about the route being followed, once per frame.
fn navigate(game: &crate::game::Game<'_>, client: &mut runtime::Client, now_ms: u64) {
    collect_search(client, now_ms);
    let player = local_player(game);
    let here = player
        .as_ref()
        .filter(|_| on_network_dimension(player.as_ref()))
        .map(|player| routing::Point { x: player.x, z: player.z });
    if client.workspace.take_route_dropped() {
        client.navigator.clear();
        show_route(client);
    }
    if let (Some(stops), Some(from)) = (client.workspace.take_route_request(), here) {
        start_search(client, from, stops.into_iter().map(|(x, z)| routing::Point { x, z }).collect());
    }
    if client.workspace.take_route_again()
        && let Some(from) = here
        && !client.navigator.stops().is_empty()
    {
        let stops = client.navigator.stops().to_vec();
        start_search(client, from, stops);
    }
    let (Some(at), true) = (here, client.navigator.route().is_some()) else {
        horizon_client_render::gfx::vexmap_render::set_marks(Default::default());
        return;
    };
    match client.navigator.sample(at, now_ms) {
        routing::Progress::Arrived => {
            client.navigator.clear();
            show_route(client);
            client.workspace.tell(editor::Notice::Arrived, now_ms);
            horizon_client_render::gfx::vexmap_render::set_marks(Default::default());
            return;
        }
        routing::Progress::StopReached => {
            show_route(client);
            client.workspace.tell(editor::Notice::StopReached, now_ms);
        }
        routing::Progress::Travelling => {}
    }
    let map_covers_the_world = is_open(game);
    let marks = player.as_ref().filter(|_| !map_covers_the_world).map_or_else(Default::default, |player| {
        horizon_client_render::gfx::vexmap_render::Marks {
            arrow: client.navigator.bearing_ahead(at).map_or_else(Vec::new, ui::arrow_mesh),
            beam: client
                .navigator
                .next_stop()
                .map_or_else(Vec::new, |end| ui::beam_mesh([player.x, player.z], [end.x, end.z])),
            origin: [player.x, player.y, player.z],
        }
    });
    horizon_client_render::gfx::vexmap_render::set_marks(marks);
    client.workspace.off_route = client.navigator.off_route();
    let russian = client.ui.settings.language == ui::Language::Ru;
    client.ui.banner = client.navigator.guidance().map(|guidance| guidance.describe(russian));
}

fn on_network_dimension(player: Option<&ui::Player>) -> bool {
    player.is_some_and(|player| player.dimension == NETWORK_DIMENSION)
}

/// Copies what the editor shows into the map's overlay, for the rebuild that follows.
fn update_overlay(
    client: &mut runtime::Client,
    size: [f32; 2],
    mouse: [f32; 2],
    player: Option<&ui::Player>,
    now: u64,
) {
    let workspace = &client.workspace;
    let (drawn, total, rendering) = workspace.render;
    let mut overlay = ui::Overlay {
        editing: workspace.editing || workspace.picking_route,
        notice: workspace.notice(now).map(editor::Notice::text),
        progress: (rendering || (workspace.may_edit && drawn < total)).then_some((drawn, total)),
        buttons: workspace
            .buttons()
            .into_iter()
            .map(|(button, lit)| ui::PanelButton { label: button.label(workspace.editing), lit })
            .collect(),
        ..ui::Overlay::default()
    };
    if workspace.editing && workspace.history_open {
        let russian = client.ui.settings.language == ui::Language::Ru;
        overlay.history = Some(ui::HistoryPanel {
            rows: workspace
                .history
                .iter()
                .map(|row| ui::HistoryLine {
                    text: row.describe(russian),
                    undone: row.undone,
                    placed: row.place.is_some(),
                })
                .collect(),
            older: workspace.history_has_older(),
            newer: workspace.history_has_newer(),
        });
    }
    if workspace.editing {
        let network = &workspace.network;
        overlay.counts = Some((network.len_nodes(), network.lines().len(), workspace.syncing()));
        if on_network_dimension(player) {
            let (min, max) = client.ui.view_box(size);
            let px_per_block = client.ui.px_per_block(size);
            let cursor = client.ui.world_at(size, mouse);
            let hover = cursor.map_or(editor::Hover::Nothing, |world| {
                workspace.editor.hover(network, (world[0], world[1]), px_per_block)
            });
            let place = |id: u64| network.node(id).map(|node| [f64::from(node.x), f64::from(node.z)]);
            overlay.lines = network
                .lines_in(min, max, MAX_DRAWN_LINES)
                .iter()
                .filter_map(|line| {
                    Some(ui::RoadLine {
                        a: place(line.a)?,
                        b: place(line.b)?,
                        kind: match line.mode {
                            editor::Mode::Walk => ui::RoadKind::Walk,
                            editor::Mode::Boat => ui::RoadKind::Boat,
                            editor::Mode::Train => ui::RoadKind::Train,
                        },
                        one_way: line.one_way,
                        closed: line.closed,
                        hot: hover == editor::Hover::Line(line.id),
                    })
                })
                .collect();
            let pending = workspace.editor.pending();
            if px_per_block >= 1.0 / MAX_NODE_BLOCKS_PER_PIXEL {
                overlay.nodes = network
                    .nodes_in(min, max, MAX_DRAWN_NODES)
                    .iter()
                    .map(|node| ui::RoadNode {
                        at: [f64::from(node.x), f64::from(node.z)],
                        station: node.transfer,
                        hot: hover == editor::Hover::Node(node.id) || pending == Some(node.id),
                    })
                    .collect();
            }
            overlay.rubber = pending.and_then(place).zip(cursor);
        } else {
            overlay.notice = Some(editor::Notice::WrongDimension.text());
        }
    }
    if workspace.picking_route {
        overlay.notice = Some(if !on_network_dimension(player) {
            editor::Notice::WrongDimension.text()
        } else if workspace.adding_stops() {
            editor::Notice::PickStop.text()
        } else {
            editor::Notice::PickDestination.text()
        });
    }
    client.ui.overlay = overlay;
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

/// With `VEXMAP_DIAG` set, the frame rate every 5 s, for the bench test. Off for players.
fn report_frame_rate(game: &crate::game::Game<'_>, client: &mut runtime::Client, now_ms: u64) {
    if !client.diagnostics || now_ms < client.next_fps_report_ms {
        return;
    }
    client.next_fps_report_ms = now_ms + 5_000;
    let (fps, _, _) = game.native_frame_rate();
    eprintln!("[vexmap] fps map={} {fps}", if is_open(game) { "open" } else { "closed" });
}

/// Every frame, on the window's game, from `runtime::frame`.
pub(super) fn frame(game: &mut crate::game::Game<'_>, client: &mut runtime::Client, now_ms: u64) {
    apply_requests(game, client);
    report_frame_rate(game, client, now_ms);
    if is_open(game) {
        let (fps, _, _) = game.native_frame_rate();
        client.ui.feed_frame_rate(now_ms, fps);
    }
    update_ground(game, client, now_ms);
    sync_network(game, client, now_ms);
    navigate(game, client, now_ms);
    client.tiles.world_changing = client.workspace.render.2;
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
                update_overlay(client, size, mouse, player.as_ref(), now);
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
        client.gui_size = size;
        client.scratch.clear();
        client.ui.banner_ops(size, 4.0, &mut client.scratch);
        replay(game, batch, &client.scratch);
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
    if button == 1 && is_open(game) {
        // The right button drops a half-drawn line, and means nothing else on the map.
        if runtime::with_client(|client| client.workspace.cancel_line()).unwrap_or(false) {
            *canceled = true;
        }
        return;
    }
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
        ui::Press::Button(index) => {
            if let Some((button, _)) = client.workspace.buttons().get(usize::from(index)) {
                client.workspace.press(*button);
            }
            *canceled = true;
        }
        ui::Press::HistoryRevert(row) => {
            client.workspace.revert(usize::from(row));
            *canceled = true;
        }
        ui::Press::HistoryShow(row) => {
            if let Some((x, z)) = client.workspace.history_place(usize::from(row)) {
                client.ui.center_on([f64::from(x), f64::from(z)]);
            }
            *canceled = true;
        }
        ui::Press::HistoryOlder => {
            client.workspace.history_older();
            *canceled = true;
        }
        ui::Press::HistoryNewer => {
            client.workspace.history_newer();
            *canceled = true;
        }
        ui::Press::Back => {}
    });
}

/// A click on the map, in the editor. The edit is sent on the next frame: this hook cannot write to the game.
fn edit_click(game: &crate::game::Game<'_>, client: &mut runtime::Client, world: [f64; 2], now: u64) {
    if !client.ui.in_world(world) {
        client.workspace.outside_world(now);
        return;
    }
    let on_network = on_network_dimension(local_player(game).as_ref());
    let px_per_block = client.ui.px_per_block(client.gui_size);
    client.workspace.click((world[0], world[1]), px_per_block, on_network, now);
}

pub(super) fn mouse_released(game: &crate::game::Game<'_>, button: i32) {
    if button != 0 || !is_open(game) {
        return;
    }
    let now = runtime::now_ms();
    runtime::with_client(|client| {
        if let Some(world) = client.ui.release() {
            edit_click(game, client, world, now);
        }
    });
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
        ui::Press::Ignored
        | ui::Press::Button(_)
        | ui::Press::HistoryRevert(_)
        | ui::Press::HistoryShow(_)
        | ui::Press::HistoryOlder
        | ui::Press::HistoryNewer => {}
        ui::Press::Taken => *canceled = true,
        ui::Press::Close => {
            client.close_requested = true;
            *canceled = true;
        }
        ui::Press::Back => {
            if client.workspace.step_back() {
                *canceled = true;
            }
        }
    });
}
