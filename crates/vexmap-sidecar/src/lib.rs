//! The local REST API. A thin layer: it parses, limits and answers, and [`vexmap_core::Core`] decides.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use vexmap_core::{
    Actor, Category, ClientName, CommandContext, Core, CoreError, CountryId, Module, RegionId, RequestId, Role,
};

pub mod fanout;
pub mod feed;
pub mod tiles;

pub use feed::PositionStore;

const MAX_BODY: usize = 64 * 1024;
const MAX_CLIENTS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub addr: SocketAddr,
    pub feed_addr: SocketAddr,
    pub owner: Option<Uuid>,
    pub rate: RateLimit,
    pub tiles: tiles::TileConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub per_second: u32,
    pub burst: u32,
}

impl Config {
    /// Reads the settings through `var`. A non-loopback address is an error: this API has no authentication.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let database_url = var("VEXMAP_DATABASE_URL").unwrap_or_else(|| "sqlite:vexmap-dev.db?mode=rwc".to_owned());
        let loopback = |name: &str, default: &str| -> Result<SocketAddr, String> {
            let text = var(name).unwrap_or_else(|| default.to_owned());
            let addr: SocketAddr = text.parse().map_err(|_| format!("{name} is not an address: {text}"))?;
            if !addr.ip().is_loopback() {
                return Err(format!("{name} must be a loopback address, got {addr}"));
            }
            Ok(addr)
        };
        let addr = loopback("VEXMAP_API_ADDR", "127.0.0.1:7878")?;
        let feed_addr = loopback("VEXMAP_FEED_ADDR", "127.0.0.1:7879")?;
        let owner = var("VEXMAP_OWNER_UUID")
            .map(|value| value.parse().map_err(|_| "VEXMAP_OWNER_UUID is not a UUID".to_owned()))
            .transpose()?;
        let number = |name: &str, default: u32| match var(name) {
            None => Ok(default),
            Some(value) => match value.parse() {
                Ok(0) | Err(_) => Err(format!("{name} must be a positive number")),
                Ok(parsed) => Ok(parsed),
            },
        };
        let rate = RateLimit { per_second: number("VEXMAP_API_RATE", 500)?, burst: number("VEXMAP_API_BURST", 1000)? };
        let tiles = tiles::TileConfig::from_env(&var);
        Ok(Config { database_url, addr, feed_addr, owner, rate, tiles })
    }
}

/// Connects, makes sure the configured owner exists, and serves until Ctrl-C.
pub async fn run(config: Config) -> Result<(), String> {
    let core = Core::connect(&config.database_url).await.map_err(|error| format!("database: {error}"))?;
    if let Some(owner) = config.owner {
        bootstrap_owner(&core, owner).await.map_err(|error| format!("owner: {error}"))?;
    }
    let listener = tokio::net::TcpListener::bind(config.addr).await.map_err(|error| format!("bind: {error}"))?;
    let feed_listener =
        tokio::net::TcpListener::bind(config.feed_addr).await.map_err(|error| format!("bind feed: {error}"))?;
    tracing::info!("VexMap API on http://{}, player feed on {}", config.addr, config.feed_addr);
    let positions = Arc::new(PositionStore::default());
    let tile_requests =
        tiles::start(config.tiles.clone(), positions.clone()).map_err(|error| format!("tiles: {error}"))?;
    tokio::spawn(feed::serve(feed_listener, core.clone(), positions.clone(), tile_requests));
    tokio::spawn(fanout::run(core.clone(), positions.clone()));
    axum::serve(listener, router(core, config.rate, positions))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| format!("serve: {error}"))
}

/// Idempotent per owner: the request id is derived from the UUID, so a restart is a replay.
pub async fn bootstrap_owner(core: &Core, owner: Uuid) -> Result<(), CoreError> {
    let ctx = CommandContext {
        actor: Actor::System(ClientName::new("sidecar")?),
        region: RegionId::global(),
        request_id: RequestId::new(&format!("bootstrap-owner-{owner}"))?,
    };
    core.set_role(&ctx, owner, Role::Owner, BTreeSet::new()).await?;
    Ok(())
}

#[derive(Clone)]
struct AppState {
    core: Core,
    limiter: Arc<Limiter>,
    positions: Arc<PositionStore>,
}

pub fn router(core: Core, rate: RateLimit, positions: Arc<PositionStore>) -> Router {
    let state = AppState { core, limiter: Arc::new(Limiter::new(rate)), positions };
    let api = Router::new()
        .route("/positions", get(get_positions))
        .route("/players/{id}", get(get_player).put(put_player))
        .route("/players/{id}/role", put(put_role))
        .route("/players/{id}/country", put(put_country))
        .route("/features", get(get_features))
        .route("/features/{module}", put(put_feature))
        .route("/journal", get(get_journal))
        .route_layer(middleware::from_fn_with_state(state.clone(), limit_client));
    Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", api)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(loopback_host_only))
        .with_state(state)
}

/// Refuses requests whose `Host` is not a loopback name, so a web page cannot reach this API by rebinding its DNS name
/// to 127.0.0.1.
async fn loopback_host_only(request: Request, next: Next) -> Response {
    let host = request.headers().get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or("");
    if is_loopback_host(host) {
        next.run(request).await
    } else {
        ApiError::new(StatusCode::FORBIDDEN, "bad_host", "this API only answers on a loopback name").into_response()
    }
}

fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    name == "localhost" || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn limit_client(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let client = match client_name(request.headers()) {
        Ok(client) => client,
        Err(error) => return error.into_response(),
    };
    if state.limiter.allow(client.as_str(), Instant::now()) {
        next.run(request).await
    } else {
        tracing::warn!(client = client.as_str(), "rate limited");
        ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many requests from this client")
            .into_response()
    }
}

/// One token bucket per client name. The names come from the caller, so their number is capped.
struct Limiter {
    rate: RateLimit,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl Limiter {
    fn new(rate: RateLimit) -> Self {
        Limiter { rate, buckets: Mutex::new(HashMap::new()) }
    }

    fn allow(&self, client: &str, now: Instant) -> bool {
        let Ok(mut buckets) = self.buckets.lock() else { return false };
        let burst = f64::from(self.rate.burst);
        if !buckets.contains_key(client) {
            if buckets.len() >= MAX_CLIENTS {
                return false;
            }
            buckets.insert(client.to_owned(), (burst, now));
        }
        let Some((tokens, last)) = buckets.get_mut(client) else { return false };
        let refill = now.saturating_duration_since(*last).as_secs_f64() * f64::from(self.rate.per_second);
        *tokens = (*tokens + refill).min(burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into() }
    }

    fn not_found(what: &str) -> Self {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("no such {what}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code, "message": self.message }))).into_response()
    }
}

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, "invalid", message),
            CoreError::Forbidden(message) => ApiError::new(StatusCode::FORBIDDEN, "forbidden", message),
            CoreError::LastOwner => ApiError::new(StatusCode::CONFLICT, "last_owner", CoreError::LastOwner.to_string()),
            other => {
                tracing::error!("{other}");
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error, see the sidecar log")
            }
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<Option<&'a str>> {
    headers
        .get(name)
        .map(|value| {
            value.to_str().map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid", format!("{name} is not text")))
        })
        .transpose()
}

fn client_name(headers: &HeaderMap) -> ApiResult<ClientName> {
    let name = header_value(headers, "x-vexmap-client")?
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid", "X-VexMap-Client is required"))?;
    Ok(ClientName::new(name)?)
}

fn parse_uuid(value: &str) -> ApiResult<Uuid> {
    value.parse().map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid", "not a UUID"))
}

/// The context of a write. With `actor` the call is on a player's behalf and that player's role decides.
fn command_context(headers: &HeaderMap, actor: Option<Uuid>) -> ApiResult<CommandContext> {
    let client = client_name(headers)?;
    let request_id = header_value(headers, "idempotency-key")?
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid", "Idempotency-Key is required"))?;
    let region = match header_value(headers, "x-vexmap-region")? {
        Some(region) => RegionId::new(region)?,
        None => RegionId::global(),
    };
    Ok(CommandContext {
        actor: actor.map_or(Actor::System(client), Actor::Player),
        region,
        request_id: RequestId::new(request_id)?,
    })
}

async fn healthz(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let head = state.core.journal_head().await?;
    Ok(Json(json!({ "status": "ok", "journal_head": head })))
}

async fn get_player(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    let player = state.core.player(parse_uuid(&id)?).await?.ok_or_else(|| ApiError::not_found("player"))?;
    Ok(Json(player))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutPlayer {
    name: String,
}

async fn put_player(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PutPlayer>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, None)?;
    Ok(Json(state.core.record_player(&ctx, parse_uuid(&id)?, &body.name).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutRole {
    role: Role,
    #[serde(default)]
    categories: BTreeSet<Category>,
    actor: Option<Uuid>,
}

async fn put_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PutRole>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, body.actor)?;
    Ok(Json(state.core.set_role(&ctx, parse_uuid(&id)?, body.role, body.categories).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutCountry {
    /// Required, with `null` meaning no country: a forgotten field must not remove a player from their country.
    #[serde(deserialize_with = "Option::deserialize")]
    country: Option<String>,
    actor: Option<Uuid>,
}

async fn put_country(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PutCountry>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, body.actor)?;
    let country = body.country.as_deref().map(CountryId::new).transpose()?;
    Ok(Json(state.core.set_country(&ctx, parse_uuid(&id)?, country).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeaturesQuery {
    region: Option<String>,
}

/// For local mods, which are trusted (assumption 10). Visibility rules belong to what is sent on to players.
async fn get_positions(
    State(state): State<AppState>,
    Query(query): Query<FeaturesQuery>,
) -> ApiResult<impl IntoResponse> {
    let region = query.region.as_deref().map(RegionId::new).transpose()?;
    Ok(Json(json!({ "regions": state.positions.snapshot(region.as_ref()) })))
}

async fn get_features(
    State(state): State<AppState>,
    Query(query): Query<FeaturesQuery>,
) -> ApiResult<impl IntoResponse> {
    let region = match query.region {
        Some(region) => RegionId::new(&region)?,
        None => RegionId::global(),
    };
    let modules = state.core.enabled_modules(&region).await?;
    let mask = modules.iter().fold(0u64, |mask, module| mask | module.bit());
    Ok(Json(json!({ "region": region, "mask": mask, "modules": modules })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutFeature {
    enabled: bool,
    region: Option<String>,
    actor: Option<Uuid>,
}

async fn put_feature(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(module): Path<String>,
    Json(body): Json<PutFeature>,
) -> ApiResult<impl IntoResponse> {
    let ctx = command_context(&headers, body.actor)?;
    let module = Module::parse(&module).ok_or_else(|| ApiError::not_found("module"))?;
    let region = match body.region {
        Some(region) => RegionId::new(&region)?,
        None => RegionId::global(),
    };
    Ok(Json(state.core.set_feature(&ctx, module, &region, body.enabled).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalQuery {
    #[serde(default)]
    after: i64,
    limit: Option<i64>,
}

async fn get_journal(State(state): State<AppState>, Query(query): Query<JournalQuery>) -> ApiResult<impl IntoResponse> {
    let entries = state.core.journal(query.after, query.limit.unwrap_or(vexmap_core::MAX_JOURNAL_PAGE)).await?;
    Ok(Json(json!({ "entries": entries })))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
    }

    #[test]
    fn config_has_safe_defaults() {
        let config = Config::from_env(env(&[])).unwrap();
        assert!(config.addr.ip().is_loopback());
        assert_eq!(config.owner, None);
    }

    #[test]
    fn config_refuses_a_public_address() {
        assert!(Config::from_env(env(&[("VEXMAP_API_ADDR", "0.0.0.0:7878")])).is_err());
        assert!(Config::from_env(env(&[("VEXMAP_API_ADDR", "192.168.1.5:7878")])).is_err());
        assert!(Config::from_env(env(&[("VEXMAP_API_ADDR", "[::1]:7878")])).is_ok());
        assert!(Config::from_env(env(&[("VEXMAP_FEED_ADDR", "0.0.0.0:7879")])).is_err());
    }

    #[test]
    fn config_refuses_nonsense() {
        assert!(Config::from_env(env(&[("VEXMAP_OWNER_UUID", "nope")])).is_err());
        assert!(Config::from_env(env(&[("VEXMAP_API_RATE", "0")])).is_err());
    }

    #[test]
    fn only_loopback_names_pass() {
        for good in ["localhost", "localhost:7878", "127.0.0.1:7878", "[::1]:7878", "[::1]"] {
            assert!(is_loopback_host(good), "{good}");
        }
        for bad in ["", "evil.example:7878", "127.0.0.1.evil.example", "[fe80::1]:7878", "localhost.evil.example"] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }

    #[test]
    fn the_bucket_empties_and_refills() {
        let limiter = Limiter::new(RateLimit { per_second: 10, burst: 2 });
        let start = Instant::now();
        assert!(limiter.allow("a", start));
        assert!(limiter.allow("a", start));
        assert!(!limiter.allow("a", start));
        assert!(limiter.allow("b", start), "clients have separate buckets");
        assert!(limiter.allow("a", start + Duration::from_millis(100)));
    }

    #[test]
    fn invented_client_names_cannot_grow_the_table_forever() {
        let limiter = Limiter::new(RateLimit { per_second: 1, burst: 1 });
        let now = Instant::now();
        for index in 0..MAX_CLIENTS {
            assert!(limiter.allow(&format!("c{index}"), now));
        }
        assert!(!limiter.allow("one-too-many", now));
        assert!(limiter.allow("c0", now + Duration::from_secs(2)), "known clients keep working");
    }
}
