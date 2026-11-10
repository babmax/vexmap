//! The VexMap data core. The REST API in `vexmap-sidecar` and any Rust mod sit on this one layer.
//!
//! Every change is one transaction that bumps a single counter, applies the change and appends to the journal. The
//! counter serializes writers, so journal order is commit order and a reader that remembers its last `seq` never skips
//! a row. Player positions do not come through here (they are Phase 3's stream, not durable edits).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod access;
mod types;

use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::migrate::Migrator;
use sqlx::{AnyPool, Row};

pub use types::{
    Actor, Category, ClientName, CommandContext, CountryId, Module, PlayerId, RegionId, RequestId, Role, RouteMode,
};

static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("migrations/sqlite");
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("migrations/postgres");

/// Bounds that keep a runaway editor from filling the database and every client's memory.
pub const MAX_ROUTE_NODES: i64 = 200_000;
pub const MAX_ROUTE_LINES: i64 = 500_000;
/// Block coordinates the world allows, with room to spare.
pub const MAX_ROUTE_COORDINATE: i64 = 30_000_000;

/// The most journal rows one read returns.
pub const MAX_JOURNAL_PAGE: i64 = 1000;
/// The most entries of the network's history one read returns.
pub const MAX_ROUTE_HISTORY_PAGE: i64 = 100;

/// The newest change to the network. Removals count, or rolling back the last row would look like an older database.
const ROUTE_VERSION_SQL: &str = "SELECT COALESCE(MAX(v), 0) FROM (SELECT MAX(updated_seq) AS v FROM route_nodes \
     UNION ALL SELECT MAX(updated_seq) FROM route_lines \
     UNION ALL SELECT MAX(updated_seq) FROM route_removed) AS versions";

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("the last owner cannot be removed")]
    LastOwner,
    #[error("the database holds a value this version cannot read: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
}

/// What became of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Applied {
        seq: i64,
    },
    /// This request id was already applied, as this journal entry. Nothing changed now.
    Replayed {
        seq: i64,
    },
    /// The state already was what the command asked for. Nothing was journaled.
    Unchanged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Player {
    pub id: PlayerId,
    pub name: Option<String>,
    pub role: Role,
    pub categories: BTreeSet<Category>,
    pub country: Option<CountryId>,
}

impl Player {
    fn new(id: PlayerId) -> Self {
        Player { id, name: None, role: Role::Player, categories: BTreeSet::new(), country: None }
    }

    fn to_json(&self) -> Value {
        json!({ "name": self.name, "role": self.role, "categories": self.categories, "country": self.country })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteNode {
    pub id: i64,
    pub x: i64,
    pub z: i64,
    pub transfer: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteLine {
    pub id: i64,
    pub a: i64,
    pub b: i64,
    pub mode: RouteMode,
    pub one_way: bool,
    pub closed: bool,
}

/// The whole network at one moment. `version` changes whenever any node or line does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteNetwork {
    pub version: i64,
    pub nodes: Vec<RouteNode>,
    pub lines: Vec<RouteLine>,
}

/// What changed in the network after a version, oldest first, up to a page. A client that applies these in order holds
/// the whole network. Rows come first and removals second: one id is never both in the same page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteChanges {
    /// The reader is up to date to this version once it has applied the rows.
    pub to: i64,
    /// The newest version of the network. Below the version asked about when the database was replaced.
    pub latest: i64,
    pub nodes: Vec<RouteNode>,
    pub lines: Vec<RouteLine>,
    pub removed_nodes: Vec<i64>,
    pub removed_lines: Vec<i64>,
}

/// One edit of the network, as the history screen shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteHistoryEntry {
    pub seq: i64,
    pub at_ms: i64,
    /// `player:…` or `system:…`, as in the journal.
    pub actor: String,
    /// The player's name when it is known.
    pub actor_name: Option<String>,
    pub kind: String,
    pub object_id: i64,
    /// For a line closed or opened: which of the two.
    pub closed: Option<bool>,
    /// A block on or next to the object, when one can still be found.
    pub place: Option<(i64, i64)>,
    /// The edit no longer stands: it was rolled back, or changed again since.
    pub undone: bool,
}

/// Newest first. `more` says there is an older page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteHistory {
    pub entries: Vec<RouteHistoryEntry>,
    pub more: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JournalEntry {
    pub seq: i64,
    pub at_ms: i64,
    pub region: String,
    pub actor: String,
    pub request_id: String,
    pub kind: String,
    pub object_type: String,
    pub object_id: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

/// A change about to be journaled.
struct Change<'a> {
    kind: &'a str,
    object_type: &'a str,
    object_id: &'a str,
    before: Option<Value>,
    after: Value,
}

type Tx = sqlx::Transaction<'static, sqlx::Any>;

/// An open write: the counter is bumped, so this transaction holds the write lock until it ends.
enum Write {
    Open { tx: Box<Tx>, seq: i64 },
    Replayed(i64),
}

#[derive(Clone)]
pub struct Core {
    pool: AnyPool,
}

impl Core {
    /// Connects to `sqlite:path?mode=rwc` or `postgres://…` and brings the schema up to date. Write SQLite paths without
    /// `//`: the URL parser would read `C:` in `sqlite://C:/x` as a host and port and lose the drive.
    pub async fn connect(url: &str) -> Result<Self, CoreError> {
        install_default_drivers();
        let sqlite = url.starts_with("sqlite:");
        if !sqlite && !url.starts_with("postgres:") && !url.starts_with("postgresql:") {
            return Err(CoreError::Invalid("the database url must start with sqlite: or postgres:".to_owned()));
        }
        let pool = AnyPoolOptions::new().max_connections(if sqlite { 4 } else { 16 }).connect(url).await?;
        if sqlite {
            // Readers keep going while the one writer works. The mode is stored in the file, so once is enough.
            sqlx::query("PRAGMA journal_mode = WAL").fetch_all(&pool).await?;
            SQLITE_MIGRATIONS.run(&pool).await?;
        } else {
            POSTGRES_MIGRATIONS.run(&pool).await?;
        }
        Ok(Core { pool })
    }

    /// The `seq` of the newest journal entry, 0 when empty.
    pub async fn journal_head(&self) -> Result<i64, CoreError> {
        Ok(sqlx::query_scalar("SELECT seq FROM journal_head").fetch_one(&self.pool).await?)
    }

    /// Entries after `after`, oldest first, at most `limit` (capped at [`MAX_JOURNAL_PAGE`]).
    pub async fn journal(&self, after: i64, limit: i64) -> Result<Vec<JournalEntry>, CoreError> {
        let rows = sqlx::query(
            "SELECT seq, at_ms, region, actor, request_id, kind, object_type, object_id, before_json, after_json \
             FROM journal WHERE seq > $1 ORDER BY seq LIMIT $2",
        )
        .bind(after)
        .bind(limit.clamp(0, MAX_JOURNAL_PAGE))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(JournalEntry {
                    seq: row.try_get(0)?,
                    at_ms: row.try_get(1)?,
                    region: row.try_get(2)?,
                    actor: row.try_get(3)?,
                    request_id: row.try_get(4)?,
                    kind: row.try_get(5)?,
                    object_type: row.try_get(6)?,
                    object_id: row.try_get(7)?,
                    before: parse_json(row.try_get(8)?)?,
                    after: parse_json(row.try_get(9)?)?,
                })
            })
            .collect()
    }

    pub async fn player(&self, id: PlayerId) -> Result<Option<Player>, CoreError> {
        let row = sqlx::query("SELECT name, role, categories, country FROM players WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| player_from_row(id, &row)).transpose()
    }

    /// Records a player's current name, typically on login. Only a local mod may do this: it is what saw the login.
    pub async fn record_player(&self, ctx: &CommandContext, id: PlayerId, name: &str) -> Result<Outcome, CoreError> {
        types::validate_player_name(name)?;
        if matches!(ctx.actor, Actor::Player(_)) {
            return Err(CoreError::Forbidden("only a local mod can record a player"));
        }
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        let before = load_player(&mut tx, id).await?;
        if before.as_ref().is_some_and(|player| player.name.as_deref() == Some(name)) {
            tx.rollback().await?;
            return Ok(Outcome::Unchanged);
        }
        let mut after = before.clone().unwrap_or_else(|| Player::new(id));
        after.name = Some(name.to_owned());
        store_player(&mut tx, &after, seq).await?;
        let change = Change {
            kind: "player.recorded",
            object_type: "player",
            object_id: &id.to_string(),
            before: before.as_ref().map(Player::to_json),
            after: after.to_json(),
        };
        finish(tx, ctx, seq, change).await
    }

    /// Changes a player's role. The player need not have joined yet: the owner is set up before the first login.
    pub async fn set_role(
        &self,
        ctx: &CommandContext,
        target: PlayerId,
        role: Role,
        categories: BTreeSet<Category>,
    ) -> Result<Outcome, CoreError> {
        if (role == Role::Moderator) == categories.is_empty() {
            return Err(CoreError::Invalid("categories are required for a moderator and only for a moderator".into()));
        }
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        let before = load_player(&mut tx, target).await?;
        let current = before.as_ref().map_or(Role::Player, |player| player.role);
        let actor = actor_role(&mut tx, &ctx.actor).await?;
        access::check_set_role(actor, current, role)?;
        if current == Role::Owner && role != Role::Owner && count_owners(&mut tx).await? == 1 {
            return Err(CoreError::LastOwner);
        }
        let mut after = before.clone().unwrap_or_else(|| Player::new(target));
        if after.role == role && after.categories == categories && before.is_some() {
            tx.rollback().await?;
            return Ok(Outcome::Unchanged);
        }
        after.role = role;
        after.categories = categories;
        store_player(&mut tx, &after, seq).await?;
        let change = Change {
            kind: "player.role_changed",
            object_type: "player",
            object_id: &target.to_string(),
            before: before.as_ref().map(Player::to_json),
            after: after.to_json(),
        };
        finish(tx, ctx, seq, change).await
    }

    /// Puts a player in a country, or in none. The player need not have joined yet.
    pub async fn set_country(
        &self,
        ctx: &CommandContext,
        target: PlayerId,
        country: Option<CountryId>,
    ) -> Result<Outcome, CoreError> {
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        access::check_set_country(actor_role(&mut tx, &ctx.actor).await?)?;
        let before = load_player(&mut tx, target).await?;
        if before.as_ref().map_or(country.is_none(), |player| player.country == country) {
            tx.rollback().await?;
            return Ok(Outcome::Unchanged);
        }
        let mut after = before.clone().unwrap_or_else(|| Player::new(target));
        after.country = country;
        store_player(&mut tx, &after, seq).await?;
        let change = Change {
            kind: "player.country_changed",
            object_type: "player",
            object_id: &target.to_string(),
            before: before.as_ref().map(Player::to_json),
            after: after.to_json(),
        };
        finish(tx, ctx, seq, change).await
    }

    /// Switches a module, for one region or for [`RegionId::global`].
    pub async fn set_feature(
        &self,
        ctx: &CommandContext,
        module: Module,
        region: &RegionId,
        enabled: bool,
    ) -> Result<Outcome, CoreError> {
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        access::check_set_feature(actor_role(&mut tx, &ctx.actor).await?)?;
        let before: Option<i64> =
            sqlx::query_scalar("SELECT enabled FROM feature_flags WHERE module = $1 AND region = $2")
                .bind(module.as_str())
                .bind(region.as_str())
                .fetch_optional(&mut **tx)
                .await?;
        if before == Some(i64::from(enabled)) {
            tx.rollback().await?;
            return Ok(Outcome::Unchanged);
        }
        sqlx::query(
            "INSERT INTO feature_flags (module, region, enabled, updated_seq) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (module, region) DO UPDATE SET enabled = excluded.enabled, updated_seq = excluded.updated_seq",
        )
        .bind(module.as_str())
        .bind(region.as_str())
        .bind(i64::from(enabled))
        .bind(seq)
        .execute(&mut **tx)
        .await?;
        let object_id = format!("{}/{}", module.as_str(), region.as_str());
        let change = Change {
            kind: "feature.changed",
            object_type: "feature",
            object_id: &object_id,
            before: before.map(|value| json!({ "enabled": value != 0 })),
            after: json!({ "enabled": enabled }),
        };
        finish(tx, ctx, seq, change).await
    }

    /// The modules on in `region`. A region's own flag wins over the global one, and a module nobody set is off.
    pub async fn enabled_modules(&self, region: &RegionId) -> Result<BTreeSet<Module>, CoreError> {
        let rows = sqlx::query("SELECT module, region, enabled FROM feature_flags WHERE region = $1 OR region = $2")
            .bind(region.as_str())
            .bind(RegionId::global().as_str())
            .fetch_all(&self.pool)
            .await?;
        let mut flags: HashMap<Module, (bool, bool)> = HashMap::new();
        for row in &rows {
            // A flag for a module this version does not know, written by a newer one, simply does not apply.
            let Some(module) = Module::parse(row.try_get::<&str, _>(0)?) else { continue };
            let own = row.try_get::<&str, _>(1)? == region.as_str();
            let enabled = row.try_get::<i64, _>(2)? != 0;
            let flag = flags.entry(module).or_insert((own, enabled));
            if own {
                *flag = (true, enabled);
            }
        }
        Ok(flags.into_iter().filter(|(_, (_, enabled))| *enabled).map(|(module, _)| module).collect())
    }

    /// The newest change to the network, 0 when it is empty. Cheap, so a cache can ask before rebuilding.
    pub async fn route_network_version(&self) -> Result<i64, CoreError> {
        Ok(sqlx::query_scalar(ROUTE_VERSION_SQL).fetch_one(&self.pool).await?)
    }

    /// The network as of one snapshot, nodes and lines ordered by id.
    pub async fn route_network(&self) -> Result<RouteNetwork, CoreError> {
        let mut tx = self.pool.begin().await?;
        let version: i64 = sqlx::query_scalar(ROUTE_VERSION_SQL).fetch_one(&mut *tx).await?;
        let node_rows =
            sqlx::query("SELECT id, x, z, transfer FROM route_nodes ORDER BY id").fetch_all(&mut *tx).await?;
        let line_rows = sqlx::query("SELECT id, a, b, mode, one_way, closed FROM route_lines ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;
        tx.rollback().await?;
        let nodes = node_rows.iter().map(route_node).collect::<Result<Vec<_>, CoreError>>()?;
        let lines = line_rows.iter().map(route_line).collect::<Result<Vec<_>, CoreError>>()?;
        Ok(RouteNetwork { version, nodes, lines })
    }

    /// At most `limit` rows changed after `since`, the oldest first. Each edit touches one row, so a page never cuts an
    /// edit in half.
    pub async fn route_network_changes(&self, since: i64, limit: usize) -> Result<RouteChanges, CoreError> {
        let limit = limit.max(1);
        let mut tx = self.pool.begin().await?;
        let latest: i64 = sqlx::query_scalar(ROUTE_VERSION_SQL).fetch_one(&mut *tx).await?;
        let node_rows = sqlx::query(
            "SELECT id, x, z, transfer, updated_seq FROM route_nodes WHERE updated_seq > $1 ORDER BY updated_seq LIMIT $2",
        )
        .bind(since)
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await?;
        let line_rows = sqlx::query(
            "SELECT id, a, b, mode, one_way, closed, updated_seq FROM route_lines WHERE updated_seq > $1 \
             ORDER BY updated_seq LIMIT $2",
        )
        .bind(since)
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await?;
        let removed_rows = sqlx::query(
            "SELECT object_type, id, updated_seq FROM route_removed WHERE updated_seq > $1 ORDER BY updated_seq LIMIT $2",
        )
        .bind(since)
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await?;
        tx.rollback().await?;

        let mut seqs = Vec::with_capacity(node_rows.len() + line_rows.len() + removed_rows.len());
        for row in &node_rows {
            seqs.push(row.try_get::<i64, _>(4)?);
        }
        for row in &line_rows {
            seqs.push(row.try_get::<i64, _>(6)?);
        }
        for row in &removed_rows {
            seqs.push(row.try_get::<i64, _>(2)?);
        }
        seqs.sort_unstable();
        // Each list is complete up to its own last row, so both are complete up to the page's last seq.
        let to = if seqs.len() >= limit { seqs[limit - 1] } else { latest };
        let mut nodes = Vec::new();
        for row in &node_rows {
            if row.try_get::<i64, _>(4)? <= to {
                nodes.push(route_node(row)?);
            }
        }
        let mut lines = Vec::new();
        for row in &line_rows {
            if row.try_get::<i64, _>(6)? <= to {
                lines.push(route_line(row)?);
            }
        }
        let (mut removed_nodes, mut removed_lines) = (Vec::new(), Vec::new());
        for row in &removed_rows {
            if row.try_get::<i64, _>(2)? <= to {
                let id = row.try_get::<i64, _>(1)?;
                match row.try_get::<&str, _>(0)? {
                    ROUTE_NODE => removed_nodes.push(id),
                    _ => removed_lines.push(id),
                }
            }
        }
        Ok(RouteChanges { to, latest, nodes, lines, removed_nodes, removed_lines })
    }

    /// The edits of the network older than `before`, newest first (0 starts from the newest), at most `limit`.
    pub async fn route_history(&self, before: i64, limit: i64) -> Result<RouteHistory, CoreError> {
        let before = if before <= 0 { i64::MAX } else { before };
        let limit = limit.clamp(1, MAX_ROUTE_HISTORY_PAGE);
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT seq, at_ms, actor, kind, object_id, before_json, after_json FROM journal \
             WHERE seq < $1 AND kind LIKE 'route.%' ORDER BY seq DESC LIMIT $2",
        )
        .bind(before)
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await?;
        let more = rows.len() as i64 > limit;
        let mut names: HashMap<String, Option<String>> = HashMap::new();
        let mut entries = Vec::with_capacity(rows.len().min(limit as usize));
        for row in rows.iter().take(limit as usize) {
            let actor: String = row.try_get(2)?;
            if !names.contains_key(&actor) {
                let name = player_name(&mut tx, &actor).await?;
                names.insert(actor.clone(), name);
            }
            let kind: String = row.try_get(3)?;
            let object_id = parse_object_id(row.try_get(4)?)?;
            let before_json = parse_json(row.try_get(5)?)?;
            let after_json = parse_json(row.try_get(6)?)?;
            let closed = after_json.as_ref().and_then(|value| value["closed"].as_bool());
            let (place, undone) =
                route_edit_state(&mut tx, &kind, object_id, before_json.as_ref(), after_json.as_ref()).await?;
            entries.push(RouteHistoryEntry {
                seq: row.try_get(0)?,
                at_ms: row.try_get(1)?,
                actor_name: names.get(&actor).cloned().flatten(),
                actor,
                kind,
                object_id,
                closed,
                place,
                undone,
            });
        }
        tx.rollback().await?;
        Ok(RouteHistory { entries, more })
    }

    /// Undoes one edit of the network. The undoing is an edit of its own, journaled and undoable in turn, so nothing is
    /// ever lost by a misclick. A node that still has lines stays until those are rolled back.
    pub async fn revert_route_edit(&self, ctx: &CommandContext, target: i64) -> Result<Outcome, CoreError> {
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        access::check_edit_network(actor_role(&mut tx, &ctx.actor).await?)?;
        let row = sqlx::query("SELECT kind, object_id, before_json FROM journal WHERE seq = $1")
            .bind(target)
            .fetch_optional(&mut **tx)
            .await?;
        let Some(row) = row else { return Err(CoreError::Invalid(format!("no journal entry {target}"))) };
        let kind: String = row.try_get(0)?;
        if !kind.starts_with("route.") {
            return Err(CoreError::Invalid("only edits of the route network can be rolled back here".into()));
        }
        let id = parse_object_id(row.try_get(1)?)?;
        let before = parse_json(row.try_get(2)?)?;
        match kind.as_str() {
            "route.node_added" | "route.node_restored" => remove_route_node(tx, ctx, seq, target, id).await,
            "route.line_added" | "route.line_restored" => remove_route_line(tx, ctx, seq, target, id).await,
            "route.line_closed_changed" => {
                let closed = before.as_ref().and_then(|value| value["closed"].as_bool());
                let Some(closed) = closed else { return Err(CoreError::Corrupt(format!("journal entry {target}"))) };
                restore_line_closed(tx, ctx, seq, target, id, closed).await
            }
            "route.node_removed" => restore_route_node(tx, ctx, seq, target, id, before).await,
            "route.line_removed" => restore_route_line(tx, ctx, seq, target, id, before).await,
            _ => Err(CoreError::Invalid("only edits of the route network can be rolled back here".into())),
        }
    }

    /// Whether this player's role allows drawing the network. The commands check again, this is for showing the editor.
    pub async fn may_edit_network(&self, player: PlayerId) -> Result<bool, CoreError> {
        let mut tx = self.pool.begin().await?;
        let role = actor_role(&mut tx, &Actor::Player(player)).await?;
        tx.rollback().await?;
        Ok(access::check_edit_network(role).is_ok())
    }

    /// Adds a node (a junction, a bend, or with `transfer` a station where the mode may change). Its id is the returned
    /// `seq`. Two nodes cannot share a block.
    pub async fn add_route_node(
        &self,
        ctx: &CommandContext,
        x: i64,
        z: i64,
        transfer: bool,
    ) -> Result<Outcome, CoreError> {
        if x.abs() > MAX_ROUTE_COORDINATE || z.abs() > MAX_ROUTE_COORDINATE {
            return Err(CoreError::Invalid("coordinate is outside the world".into()));
        }
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        access::check_edit_network(actor_role(&mut tx, &ctx.actor).await?)?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_nodes").fetch_one(&mut **tx).await?;
        if count >= MAX_ROUTE_NODES {
            return Err(CoreError::Invalid("the network has the most nodes it may".into()));
        }
        let taken: Option<i64> = sqlx::query_scalar("SELECT id FROM route_nodes WHERE x = $1 AND z = $2")
            .bind(x)
            .bind(z)
            .fetch_optional(&mut **tx)
            .await?;
        if taken.is_some() {
            return Err(CoreError::Invalid("there already is a node at this block".into()));
        }
        sqlx::query("INSERT INTO route_nodes (id, x, z, transfer, updated_seq) VALUES ($1, $2, $3, $4, $5)")
            .bind(seq)
            .bind(x)
            .bind(z)
            .bind(i64::from(transfer))
            .bind(seq)
            .execute(&mut **tx)
            .await?;
        let object_id = seq.to_string();
        let change = Change {
            kind: "route.node_added",
            object_type: "route_node",
            object_id: &object_id,
            before: None,
            after: json!({ "x": x, "z": z, "transfer": transfer }),
        };
        finish(tx, ctx, seq, change).await
    }

    /// Draws a line between two existing nodes. Its id is the returned `seq`. A pair of nodes has at most one line per
    /// mode, whichever way round, so a second drawing is refused instead of doubling the road.
    pub async fn add_route_line(
        &self,
        ctx: &CommandContext,
        a: i64,
        b: i64,
        mode: RouteMode,
        one_way: bool,
    ) -> Result<Outcome, CoreError> {
        if a == b {
            return Err(CoreError::Invalid("a line needs two different nodes".into()));
        }
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        access::check_edit_network(actor_role(&mut tx, &ctx.actor).await?)?;
        for node in [a, b] {
            let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM route_nodes WHERE id = $1")
                .bind(node)
                .fetch_optional(&mut **tx)
                .await?;
            if exists.is_none() {
                return Err(CoreError::Invalid(format!("no node {node}")));
            }
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_lines").fetch_one(&mut **tx).await?;
        if count >= MAX_ROUTE_LINES {
            return Err(CoreError::Invalid("the network has the most lines it may".into()));
        }
        let twin: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM route_lines WHERE mode = $1 AND ((a = $2 AND b = $3) OR (a = $3 AND b = $2))",
        )
        .bind(mode.as_str())
        .bind(a)
        .bind(b)
        .fetch_optional(&mut **tx)
        .await?;
        if twin.is_some() {
            return Err(CoreError::Invalid("these nodes already have a line of this mode".into()));
        }
        sqlx::query(
            "INSERT INTO route_lines (id, a, b, mode, one_way, closed, updated_seq) VALUES ($1, $2, $3, $4, $5, 0, $6)",
        )
        .bind(seq)
        .bind(a)
        .bind(b)
        .bind(mode.as_str())
        .bind(i64::from(one_way))
        .bind(seq)
        .execute(&mut **tx)
        .await?;
        let object_id = seq.to_string();
        let change = Change {
            kind: "route.line_added",
            object_type: "route_line",
            object_id: &object_id,
            before: None,
            after: json!({ "a": a, "b": b, "mode": mode, "one_way": one_way, "closed": false }),
        };
        finish(tx, ctx, seq, change).await
    }

    /// Closes or reopens a line (works, a closed border). Closed lines stay in the network and are skipped by routes.
    pub async fn set_route_line_closed(
        &self,
        ctx: &CommandContext,
        line: i64,
        closed: bool,
    ) -> Result<Outcome, CoreError> {
        let (mut tx, seq) = match self.begin_write(ctx).await? {
            Write::Open { tx, seq } => (tx, seq),
            Write::Replayed(seq) => return Ok(Outcome::Replayed { seq }),
        };
        access::check_edit_network(actor_role(&mut tx, &ctx.actor).await?)?;
        let before: Option<i64> = sqlx::query_scalar("SELECT closed FROM route_lines WHERE id = $1")
            .bind(line)
            .fetch_optional(&mut **tx)
            .await?;
        let Some(before) = before else { return Err(CoreError::Invalid(format!("no line {line}"))) };
        if before == i64::from(closed) {
            tx.rollback().await?;
            return Ok(Outcome::Unchanged);
        }
        sqlx::query("UPDATE route_lines SET closed = $1, updated_seq = $2 WHERE id = $3")
            .bind(i64::from(closed))
            .bind(seq)
            .bind(line)
            .execute(&mut **tx)
            .await?;
        let object_id = line.to_string();
        let change = Change {
            kind: "route.line_closed_changed",
            object_type: "route_line",
            object_id: &object_id,
            before: Some(json!({ "closed": before != 0 })),
            after: json!({ "closed": closed }),
        };
        finish(tx, ctx, seq, change).await
    }

    /// Takes the write lock and checks the request id. The bump comes first so that a read inside the transaction can
    /// never be stale: on SQLite a transaction that starts by reading cannot wait for the lock.
    async fn begin_write(&self, ctx: &CommandContext) -> Result<Write, CoreError> {
        let mut tx = self.pool.begin().await?;
        let seq: i64 =
            sqlx::query_scalar("UPDATE journal_head SET seq = seq + 1 RETURNING seq").fetch_one(&mut *tx).await?;
        let earlier: Option<i64> = sqlx::query_scalar("SELECT seq FROM journal WHERE actor = $1 AND request_id = $2")
            .bind(ctx.actor.to_string())
            .bind(ctx.request_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(earlier) = earlier {
            tx.rollback().await?;
            return Ok(Write::Replayed(earlier));
        }
        Ok(Write::Open { tx: Box::new(tx), seq })
    }
}

/// A row whose first columns are `id, x, z, transfer`.
fn route_node(row: &sqlx::any::AnyRow) -> Result<RouteNode, CoreError> {
    Ok(RouteNode {
        id: row.try_get(0)?,
        x: row.try_get(1)?,
        z: row.try_get(2)?,
        transfer: row.try_get::<i64, _>(3)? != 0,
    })
}

/// A row whose first columns are `id, a, b, mode, one_way, closed`.
fn route_line(row: &sqlx::any::AnyRow) -> Result<RouteLine, CoreError> {
    let mode: String = row.try_get(3)?;
    Ok(RouteLine {
        id: row.try_get(0)?,
        a: row.try_get(1)?,
        b: row.try_get(2)?,
        mode: RouteMode::parse(&mode).ok_or_else(|| CoreError::Corrupt(format!("route mode {mode:?}")))?,
        one_way: row.try_get::<i64, _>(4)? != 0,
        closed: row.try_get::<i64, _>(5)? != 0,
    })
}

const ROUTE_NODE: &str = "route_node";
const ROUTE_LINE: &str = "route_line";

fn parse_object_id(text: String) -> Result<i64, CoreError> {
    text.parse().map_err(|_| CoreError::Corrupt(format!("object id {text:?}")))
}

async fn player_name(tx: &mut Tx, actor: &str) -> Result<Option<String>, CoreError> {
    let Some(id) = actor.strip_prefix("player:") else { return Ok(None) };
    let name: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM players WHERE id = $1").bind(id).fetch_optional(&mut **tx).await?;
    Ok(name.flatten())
}

async fn load_route_node(tx: &mut Tx, id: i64) -> Result<Option<RouteNode>, CoreError> {
    let row = sqlx::query("SELECT id, x, z, transfer FROM route_nodes WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(route_node).transpose()
}

async fn load_route_line(tx: &mut Tx, id: i64) -> Result<Option<RouteLine>, CoreError> {
    let row = sqlx::query("SELECT id, a, b, mode, one_way, closed FROM route_lines WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(route_line).transpose()
}

async fn node_place(tx: &mut Tx, id: i64) -> Result<Option<(i64, i64)>, CoreError> {
    Ok(load_route_node(tx, id).await?.map(|node| (node.x, node.z)))
}

/// Where an edit happened, and whether it no longer stands. The journal holds what a removed row looked like.
async fn route_edit_state(
    tx: &mut Tx,
    kind: &str,
    id: i64,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<(Option<(i64, i64)>, bool), CoreError> {
    let saved_place = |value: Option<&Value>| Some((value?["x"].as_i64()?, value?["z"].as_i64()?));
    let saved_end = |value: Option<&Value>| value.and_then(|value| value["a"].as_i64());
    Ok(match kind {
        "route.node_added" | "route.node_restored" => {
            let place = node_place(tx, id).await?;
            (place.or_else(|| saved_place(after)), place.is_none())
        }
        "route.node_removed" => {
            let place = node_place(tx, id).await?;
            (place.or_else(|| saved_place(before)), place.is_some())
        }
        "route.line_added" | "route.line_restored" => {
            let line = load_route_line(tx, id).await?;
            let end = line.as_ref().map(|line| line.a).or_else(|| saved_end(after));
            let place = if let Some(end) = end { node_place(tx, end).await? } else { None };
            (place, line.is_none())
        }
        "route.line_removed" => {
            let line = load_route_line(tx, id).await?;
            let end = line.as_ref().map(|line| line.a).or_else(|| saved_end(before));
            let place = if let Some(end) = end { node_place(tx, end).await? } else { None };
            (place, line.is_some())
        }
        "route.line_closed_changed" => {
            let line = load_route_line(tx, id).await?;
            let wanted = after.and_then(|value| value["closed"].as_bool());
            let place = if let Some(line) = &line { node_place(tx, line.a).await? } else { None };
            (place, line.is_none_or(|line| Some(line.closed) != wanted))
        }
        _ => (None, false),
    })
}

/// Remembers that a row is gone, for the clients that hold a copy of it.
async fn mark_removed(tx: &mut Tx, object_type: &str, id: i64, seq: i64) -> Result<(), CoreError> {
    sqlx::query(
        "INSERT INTO route_removed (object_type, id, updated_seq) VALUES ($1, $2, $3) \
         ON CONFLICT (object_type, id) DO UPDATE SET updated_seq = excluded.updated_seq",
    )
    .bind(object_type)
    .bind(id)
    .bind(seq)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn clear_removed(tx: &mut Tx, object_type: &str, id: i64) -> Result<(), CoreError> {
    sqlx::query("DELETE FROM route_removed WHERE object_type = $1 AND id = $2")
        .bind(object_type)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn remove_route_node(
    mut tx: Box<Tx>,
    ctx: &CommandContext,
    seq: i64,
    target: i64,
    id: i64,
) -> Result<Outcome, CoreError> {
    let Some(node) = load_route_node(&mut tx, id).await? else {
        tx.rollback().await?;
        return Ok(Outcome::Unchanged);
    };
    let lines: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_lines WHERE a = $1 OR b = $1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    if lines > 0 {
        return Err(CoreError::Invalid("this node still has lines: roll those back first".into()));
    }
    sqlx::query("DELETE FROM route_nodes WHERE id = $1").bind(id).execute(&mut **tx).await?;
    mark_removed(&mut tx, ROUTE_NODE, id, seq).await?;
    let object_id = id.to_string();
    let change = Change {
        kind: "route.node_removed",
        object_type: ROUTE_NODE,
        object_id: &object_id,
        before: Some(json!({ "x": node.x, "z": node.z, "transfer": node.transfer })),
        after: json!({ "removed": true, "reverts": target }),
    };
    finish(tx, ctx, seq, change).await
}

async fn remove_route_line(
    mut tx: Box<Tx>,
    ctx: &CommandContext,
    seq: i64,
    target: i64,
    id: i64,
) -> Result<Outcome, CoreError> {
    let Some(line) = load_route_line(&mut tx, id).await? else {
        tx.rollback().await?;
        return Ok(Outcome::Unchanged);
    };
    sqlx::query("DELETE FROM route_lines WHERE id = $1").bind(id).execute(&mut **tx).await?;
    mark_removed(&mut tx, ROUTE_LINE, id, seq).await?;
    let object_id = id.to_string();
    let change = Change {
        kind: "route.line_removed",
        object_type: ROUTE_LINE,
        object_id: &object_id,
        before: Some(json!({
            "a": line.a, "b": line.b, "mode": line.mode, "one_way": line.one_way, "closed": line.closed
        })),
        after: json!({ "removed": true, "reverts": target }),
    };
    finish(tx, ctx, seq, change).await
}

async fn restore_line_closed(
    mut tx: Box<Tx>,
    ctx: &CommandContext,
    seq: i64,
    target: i64,
    id: i64,
    closed: bool,
) -> Result<Outcome, CoreError> {
    let Some(line) = load_route_line(&mut tx, id).await? else {
        return Err(CoreError::Invalid(format!("no line {id}")));
    };
    if line.closed == closed {
        tx.rollback().await?;
        return Ok(Outcome::Unchanged);
    }
    sqlx::query("UPDATE route_lines SET closed = $1, updated_seq = $2 WHERE id = $3")
        .bind(i64::from(closed))
        .bind(seq)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    let object_id = id.to_string();
    let change = Change {
        kind: "route.line_closed_changed",
        object_type: ROUTE_LINE,
        object_id: &object_id,
        before: Some(json!({ "closed": line.closed })),
        after: json!({ "closed": closed, "reverts": target }),
    };
    finish(tx, ctx, seq, change).await
}

async fn restore_route_node(
    mut tx: Box<Tx>,
    ctx: &CommandContext,
    seq: i64,
    target: i64,
    id: i64,
    saved: Option<Value>,
) -> Result<Outcome, CoreError> {
    if load_route_node(&mut tx, id).await?.is_some() {
        tx.rollback().await?;
        return Ok(Outcome::Unchanged);
    }
    let corrupt = || CoreError::Corrupt(format!("journal entry {target}"));
    let saved = saved.ok_or_else(corrupt)?;
    let x = saved["x"].as_i64().ok_or_else(corrupt)?;
    let z = saved["z"].as_i64().ok_or_else(corrupt)?;
    let transfer = saved["transfer"].as_bool().ok_or_else(corrupt)?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_nodes").fetch_one(&mut **tx).await?;
    if count >= MAX_ROUTE_NODES {
        return Err(CoreError::Invalid("the network has the most nodes it may".into()));
    }
    let taken: Option<i64> = sqlx::query_scalar("SELECT id FROM route_nodes WHERE x = $1 AND z = $2")
        .bind(x)
        .bind(z)
        .fetch_optional(&mut **tx)
        .await?;
    if taken.is_some() {
        return Err(CoreError::Invalid("another node stands at this block now".into()));
    }
    sqlx::query("INSERT INTO route_nodes (id, x, z, transfer, updated_seq) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(x)
        .bind(z)
        .bind(i64::from(transfer))
        .bind(seq)
        .execute(&mut **tx)
        .await?;
    clear_removed(&mut tx, ROUTE_NODE, id).await?;
    let object_id = id.to_string();
    let change = Change {
        kind: "route.node_restored",
        object_type: ROUTE_NODE,
        object_id: &object_id,
        before: None,
        after: json!({ "x": x, "z": z, "transfer": transfer, "reverts": target }),
    };
    finish(tx, ctx, seq, change).await
}

async fn restore_route_line(
    mut tx: Box<Tx>,
    ctx: &CommandContext,
    seq: i64,
    target: i64,
    id: i64,
    saved: Option<Value>,
) -> Result<Outcome, CoreError> {
    if load_route_line(&mut tx, id).await?.is_some() {
        tx.rollback().await?;
        return Ok(Outcome::Unchanged);
    }
    let corrupt = || CoreError::Corrupt(format!("journal entry {target}"));
    let saved = saved.ok_or_else(corrupt)?;
    let a = saved["a"].as_i64().ok_or_else(corrupt)?;
    let b = saved["b"].as_i64().ok_or_else(corrupt)?;
    let mode = saved["mode"].as_str().and_then(RouteMode::parse).ok_or_else(corrupt)?;
    let one_way = saved["one_way"].as_bool().ok_or_else(corrupt)?;
    let closed = saved["closed"].as_bool().ok_or_else(corrupt)?;
    for node in [a, b] {
        if load_route_node(&mut tx, node).await?.is_none() {
            return Err(CoreError::Invalid(format!("node {node} of this line is gone: restore it first")));
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_lines").fetch_one(&mut **tx).await?;
    if count >= MAX_ROUTE_LINES {
        return Err(CoreError::Invalid("the network has the most lines it may".into()));
    }
    let twin: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM route_lines WHERE mode = $1 AND ((a = $2 AND b = $3) OR (a = $3 AND b = $2))",
    )
    .bind(mode.as_str())
    .bind(a)
    .bind(b)
    .fetch_optional(&mut **tx)
    .await?;
    if twin.is_some() {
        return Err(CoreError::Invalid("these nodes have a line of this mode again".into()));
    }
    sqlx::query(
        "INSERT INTO route_lines (id, a, b, mode, one_way, closed, updated_seq) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(a)
    .bind(b)
    .bind(mode.as_str())
    .bind(i64::from(one_way))
    .bind(i64::from(closed))
    .bind(seq)
    .execute(&mut **tx)
    .await?;
    clear_removed(&mut tx, ROUTE_LINE, id).await?;
    let object_id = id.to_string();
    let change = Change {
        kind: "route.line_restored",
        object_type: ROUTE_LINE,
        object_id: &object_id,
        before: None,
        after: json!({ "a": a, "b": b, "mode": mode, "one_way": one_way, "closed": closed, "reverts": target }),
    };
    finish(tx, ctx, seq, change).await
}

/// `None` for a local mod. A player unknown to the database has no powers.
async fn actor_role(tx: &mut Tx, actor: &Actor) -> Result<Option<Role>, CoreError> {
    match actor {
        Actor::System(_) => Ok(None),
        Actor::Player(id) => Ok(Some(load_player(tx, *id).await?.map_or(Role::Player, |player| player.role))),
    }
}

async fn count_owners(tx: &mut Tx) -> Result<i64, CoreError> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM players WHERE role = $1")
        .bind(Role::Owner.as_str())
        .fetch_one(&mut **tx)
        .await?)
}

fn player_from_row(id: PlayerId, row: &sqlx::any::AnyRow) -> Result<Player, CoreError> {
    Ok(Player {
        id,
        name: row.try_get(0)?,
        role: Role::parse(row.try_get(1)?)?,
        categories: Category::from_bits(row.try_get(2)?),
        country: row
            .try_get::<Option<String>, _>(3)?
            .map(|country| CountryId::new(&country).map_err(|_| CoreError::Corrupt(format!("country {country:?}"))))
            .transpose()?,
    })
}

async fn load_player(tx: &mut Tx, id: PlayerId) -> Result<Option<Player>, CoreError> {
    let row = sqlx::query("SELECT name, role, categories, country FROM players WHERE id = $1")
        .bind(id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| player_from_row(id, &row)).transpose()
}

async fn store_player(tx: &mut Tx, player: &Player, seq: i64) -> Result<(), CoreError> {
    sqlx::query(
        "INSERT INTO players (id, name, role, categories, country, updated_seq) VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, role = excluded.role, \
         categories = excluded.categories, country = excluded.country, updated_seq = excluded.updated_seq",
    )
    .bind(player.id.to_string())
    .bind(player.name.clone())
    .bind(player.role.as_str())
    .bind(Category::to_bits(&player.categories))
    .bind(player.country.as_ref().map(|country| country.as_str().to_owned()))
    .bind(seq)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Appends the journal entry and commits, so a change and its record can never be separated.
async fn finish(mut tx: Box<Tx>, ctx: &CommandContext, seq: i64, change: Change<'_>) -> Result<Outcome, CoreError> {
    sqlx::query(
        "INSERT INTO journal (seq, at_ms, region, actor, request_id, kind, object_type, object_id, before_json, after_json) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(seq)
    .bind(now_ms())
    .bind(ctx.region.as_str())
    .bind(ctx.actor.to_string())
    .bind(ctx.request_id.as_str())
    .bind(change.kind)
    .bind(change.object_type)
    .bind(change.object_id)
    .bind(change.before.map(|value| value.to_string()))
    .bind(change.after.to_string())
    .execute(&mut **tx)
    .await?;
    tx.commit().await?;
    Ok(Outcome::Applied { seq })
}

fn parse_json(text: Option<String>) -> Result<Option<Value>, CoreError> {
    text.map(|text| serde_json::from_str(&text).map_err(|error| CoreError::Corrupt(error.to_string()))).transpose()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as i64)
}
