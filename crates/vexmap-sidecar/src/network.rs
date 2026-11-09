//! The route network over REST (Phase 6): drawing it, reading it for clients, and building a route for a local mod.
//!
//! The graph is rebuilt only when the network's version moves, and shared between requests. The search itself runs on a
//! blocking thread so a large network cannot stall the API.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;
use vexmap_core::{Core, CoreError, RouteMode};
use vexmap_routing::{Graph, LegKind, Mode, Point, RouteError, Speeds};

use crate::{ApiError, ApiResult, AppState, command_context};

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
