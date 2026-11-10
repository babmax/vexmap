//! The route network over REST (Phase 6): drawing it, reading it for clients, and building a route for a local mod.
//!
//! The graph is rebuilt only when the network's version moves, and shared between requests. The search itself runs on a
//! blocking thread so a large network cannot stall the API.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;
use vexmap_core::{Core, CoreError, RouteMode};
use vexmap_link::{Delivery, NetworkWant, encode_tile_deliveries};
use vexmap_proto::{
    HistoryEntry, HistoryKind, LineMode, MAX_HISTORY_PAGE, MAX_NETWORK_PAGE, MAX_S2C_FRAME, Message, NetworkHistory,
    NetworkLine, NetworkNode, NetworkPage, Outbox,
};
use vexmap_routing::{Graph, LegKind, Mode, Point, RouteError, Speeds};
use vexmap_tiler::store::RenderProgress;

use crate::feed::PositionStore;
use crate::{ApiError, ApiResult, AppState, command_context};

/// Players waiting for a page of the network. Past this they are dropped, and their clients ask again.
const WANT_QUEUE: usize = 1024;

pub struct RegionWant {
    pub region: String,
    pub want: NetworkWant,
}

/// The rows after `since` for one player. A `since` past what the database can hold is simply past the newest version.
pub async fn page_for(core: &Core, want: NetworkWant, render: &RenderProgress) -> Result<NetworkPage, CoreError> {
    let changes = core.route_network_changes(i64::try_from(want.since).unwrap_or(i64::MAX), MAX_NETWORK_PAGE).await?;
    let may_edit = core.may_edit_network(Uuid::from_u128(want.viewer)).await?;
    if want.render && may_edit {
        render.request_full();
    }
    let nodes = changes
        .nodes
        .iter()
        .filter_map(|node| {
            let (x, z) = (i32::try_from(node.x).ok()?, i32::try_from(node.z).ok()?);
            Some(NetworkNode { id: node.id as u64, x, z, transfer: node.transfer })
        })
        .collect();
    let lines = changes
        .lines
        .iter()
        .map(|line| NetworkLine {
            id: line.id as u64,
            a: line.a as u64,
            b: line.b as u64,
            mode: match line.mode {
                RouteMode::Walk => LineMode::Walk,
                RouteMode::Boat => LineMode::Boat,
                RouteMode::Train => LineMode::Train,
            },
            one_way: line.one_way,
            closed: line.closed,
        })
        .collect();
    Ok(NetworkPage {
        since: want.since,
        to: changes.to as u64,
        latest: changes.latest as u64,
        may_edit,
        render_done: render.done.load(Ordering::Relaxed),
        render_total: render.total.load(Ordering::Relaxed),
        rendering: render.fast.load(Ordering::Relaxed),
        nodes,
        lines,
        removed_nodes: changes.removed_nodes.iter().map(|id| *id as u64).collect(),
        removed_lines: changes.removed_lines.iter().map(|id| *id as u64).collect(),
    })
}

fn history_kind(kind: &str, closed: Option<bool>) -> Option<HistoryKind> {
    Some(match kind {
        "route.node_added" | "route.node_restored" => HistoryKind::NodeAdded,
        "route.line_added" | "route.line_restored" => HistoryKind::LineAdded,
        "route.line_closed_changed" if closed == Some(true) => HistoryKind::LineClosed,
        "route.line_closed_changed" => HistoryKind::LineOpened,
        "route.node_removed" => HistoryKind::NodeRemoved,
        "route.line_removed" => HistoryKind::LineRemoved,
        _ => return None,
    })
}

/// A page of the network's history for one player, or `None` when the database would not let them edit.
pub async fn history_for(core: &Core, want: NetworkWant) -> Result<Option<NetworkHistory>, CoreError> {
    if !core.may_edit_network(Uuid::from_u128(want.viewer)).await? {
        return Ok(None);
    }
    let before = i64::try_from(want.since).unwrap_or(i64::MAX);
    let history = core.route_history(before, MAX_HISTORY_PAGE as i64).await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as i64);
    let entries = history
        .entries
        .iter()
        .filter_map(|entry| {
            Some(HistoryEntry {
                seq: entry.seq as u64,
                age_seconds: u32::try_from((now - entry.at_ms).max(0) / 1000).unwrap_or(u32::MAX),
                actor: entry.actor_name.clone().unwrap_or_default(),
                kind: history_kind(&entry.kind, entry.closed)?,
                object: entry.object_id as u64,
                place: entry.place.and_then(|(x, z)| Some((i32::try_from(x).ok()?, i32::try_from(z).ok()?))),
                undone: entry.undone,
            })
        })
        .collect();
    Ok(Some(NetworkHistory { before: want.since, more: history.more, entries }))
}

/// Reads one page at a time, so a crowd of clients costs the database one query at a time. The answers take the tiles'
/// way back: they are for one player, and the server limits how many bytes of them one tick sends.
async fn serve_pages(
    mut wants: mpsc::Receiver<RegionWant>,
    core: Core,
    positions: Arc<PositionStore>,
    render: Arc<RenderProgress>,
) {
    let mut outbox = Outbox::default();
    while let Some(RegionWant { region, want }) = wants.recv().await {
        let answer = if want.history {
            history_for(&core, want).await.map(|history| history.map(Message::NetworkHistory))
        } else {
            page_for(&core, want, &render).await.map(|page| Some(Message::NetworkPage(page)))
        };
        let message = match answer {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!("network page: {error}");
                continue;
            }
        };
        let Ok(frames) = outbox.frames(&message, MAX_S2C_FRAME) else { continue };
        let deliveries: Vec<Delivery> =
            frames.into_iter().map(|frame| Delivery { viewers: vec![want.viewer], frame }).collect();
        let mut bytes = Vec::new();
        encode_tile_deliveries(&deliveries, &mut bytes);
        if !positions.deliver_tiles(&region, bytes) {
            tracing::debug!(region, "network page dropped: the region's queue is full or it is gone");
        }
    }
}

/// Starts the worker and returns where requests go.
pub fn start_pages(core: Core, positions: Arc<PositionStore>, render: Arc<RenderProgress>) -> mpsc::Sender<RegionWant> {
    let (sender, receiver) = mpsc::channel(WANT_QUEUE);
    tokio::spawn(serve_pages(receiver, core, positions, render));
    sender
}

/// The graph for one network version.
#[derive(Default)]
pub(crate) struct RouteCache {
    current: Mutex<Option<(i64, Arc<Graph>)>>,
}

impl RouteCache {
    async fn graph(&self, core: &Core) -> Result<Arc<Graph>, CoreError> {
        let version = core.route_network_version().await?;
        let mut current = self.current.lock().await;
        if let Some((cached, graph)) = current.as_ref()
            && *cached == version
        {
            return Ok(graph.clone());
        }
        let network = core.route_network().await?;
        let mut graph = Graph::new();
        let mut ids = HashMap::with_capacity(network.nodes.len());
        for node in &network.nodes {
            ids.insert(node.id, graph.add_node(node.x as f64, node.z as f64, node.transfer));
        }
        for line in &network.lines {
            let (Some(a), Some(b)) = (ids.get(&line.a), ids.get(&line.b)) else { continue };
            let mode = match line.mode {
                RouteMode::Walk => Mode::Walk,
                RouteMode::Boat => Mode::Boat,
                RouteMode::Train => Mode::Train,
            };
            // Nodes exist and differ (the core checked), so this cannot fail; a damaged row is skipped, not fatal.
            let Ok(edges) = graph.add_line(*a, *b, mode, line.one_way) else { continue };
            if line.closed {
                for edge in edges {
                    graph.set_closed(edge, true);
                }
            }
        }
        let graph = Arc::new(graph);
        *current = Some((network.version, graph.clone()));
        Ok(graph)
    }
}

pub(crate) async fn get_network(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(state.core.route_network().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostNode {
    x: i64,
    z: i64,
    #[serde(default)]
    transfer: bool,
    actor: Option<Uuid>,
}

pub(crate) async fn post_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PostNode>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, body.actor)?;
    Ok(Json(state.core.add_route_node(&ctx, body.x, body.z, body.transfer).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostLine {
    a: i64,
    b: i64,
    mode: RouteMode,
    #[serde(default)]
    one_way: bool,
    actor: Option<Uuid>,
}

pub(crate) async fn post_line(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PostLine>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, body.actor)?;
    Ok(Json(state.core.add_route_line(&ctx, body.a, body.b, body.mode, body.one_way).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutClosed {
    closed: bool,
    actor: Option<Uuid>,
}

pub(crate) async fn put_line_closed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<PutClosed>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, body.actor)?;
    Ok(Json(state.core.set_route_line_closed(&ctx, id, body.closed).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryQuery {
    /// Entries older than this journal number. Missing starts from the newest.
    before: Option<i64>,
    limit: Option<i64>,
}

pub(crate) async fn get_history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<impl IntoResponse> {
    let limit = query.limit.unwrap_or(vexmap_core::MAX_ROUTE_HISTORY_PAGE);
    Ok(Json(state.core.route_history(query.before.unwrap_or(0), limit).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostRevert {
    actor: Option<Uuid>,
}

pub(crate) async fn post_revert(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(seq): Path<i64>,
    Json(body): Json<PostRevert>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, body.actor)?;
    Ok(Json(state.core.revert_route_edit(&ctx, seq).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stop {
    x: f64,
    z: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RouteRequest {
    stops: Vec<Stop>,
    /// Modes allowed. Missing means all.
    modes: Option<Vec<String>>,
    /// The player's own speeds in blocks per second, from their `SpeedTracker`. Missing keeps the defaults.
    walk_speed: Option<f64>,
    boat_speed: Option<f64>,
    train_speed: Option<f64>,
}

pub(crate) async fn post_route(
    State(state): State<AppState>,
    Json(body): Json<RouteRequest>,
) -> ApiResult<impl IntoResponse> {
    let modes = match body.modes {
        None => vec![Mode::Walk, Mode::Boat, Mode::Train],
        Some(names) => names
            .iter()
            .map(|name| {
                Mode::parse(name).ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid", "unknown mode"))
            })
            .collect::<ApiResult<Vec<_>>>()?,
    };
    let defaults = Speeds::default();
    let speeds = Speeds {
        walk: body.walk_speed.unwrap_or(defaults.walk),
        boat: body.boat_speed.unwrap_or(defaults.boat),
        train: body.train_speed.unwrap_or(defaults.train),
        ..defaults
    };
    let stops: Vec<Point> = body.stops.iter().map(|stop| Point { x: stop.x, z: stop.z }).collect();
    let graph = state.routes.graph(&state.core).await?;
    let route = tokio::task::spawn_blocking(move || graph.route(&stops, &modes, &speeds))
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "the route search failed"))?;
    let route = route.map_err(|error| match error {
        RouteError::Unreachable(stop) => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "no_route", format!("stop {stop} cannot be reached"))
        }
        RouteError::BadSpeeds => ApiError::new(StatusCode::BAD_REQUEST, "invalid", "speeds must be positive numbers"),
        RouteError::NoStops => ApiError::new(StatusCode::BAD_REQUEST, "invalid", "a route needs at least two stops"),
        RouteError::TooManyStops => ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("a route has at most {} stops", vexmap_routing::MAX_STOPS),
        ),
    })?;
    let legs: Vec<Value> = route
        .legs
        .iter()
        .map(|leg| {
            json!({
                "mode": leg.mode.as_str(),
                "kind": match leg.kind { LegKind::Network => "network", LegKind::Direct => "direct" },
                "length": leg.length,
                "seconds": leg.seconds,
                "points": leg.points.iter().map(|p| [p.x, p.z]).collect::<Vec<_>>(),
            })
        })
        .collect();
    let hints: Vec<Value> = route
        .hints()
        .iter()
        .map(|hint| json!({ "x": hint.at.x, "z": hint.at.z, "turn": hint.turn.as_str(), "after": hint.after }))
        .collect();
    Ok(Json(json!({ "length": route.length, "seconds": route.seconds, "legs": legs, "hints": hints })))
}
