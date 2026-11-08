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

pub use types::{Actor, Category, ClientName, CommandContext, Module, PlayerId, RegionId, RequestId, Role};

static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("migrations/sqlite");
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("migrations/postgres");

/// The most journal rows one read returns.
pub const MAX_JOURNAL_PAGE: i64 = 1000;

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
}

impl Player {
    fn new(id: PlayerId) -> Self {
        Player { id, name: None, role: Role::Player, categories: BTreeSet::new() }
    }

    fn to_json(&self) -> Value {
        json!({ "name": self.name, "role": self.role, "categories": self.categories })
    }
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
        let row = sqlx::query("SELECT name, role, categories FROM players WHERE id = $1")
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
    })
}

async fn load_player(tx: &mut Tx, id: PlayerId) -> Result<Option<Player>, CoreError> {
    let row = sqlx::query("SELECT name, role, categories FROM players WHERE id = $1")
        .bind(id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| player_from_row(id, &row)).transpose()
}

async fn store_player(tx: &mut Tx, player: &Player, seq: i64) -> Result<(), CoreError> {
    sqlx::query(
        "INSERT INTO players (id, name, role, categories, updated_seq) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, role = excluded.role, \
         categories = excluded.categories, updated_seq = excluded.updated_seq",
    )
    .bind(player.id.to_string())
    .bind(player.name.clone())
    .bind(player.role.as_str())
    .bind(Category::to_bits(&player.categories))
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
