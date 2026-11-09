//! Runs on SQLite always, and on PostgreSQL too when `VEXMAP_TEST_POSTGRES_URL` is set (it wipes that database's public
//! schema, so run with `--test-threads=1` and point it at a throwaway server).

use std::collections::BTreeSet;
use std::future::Future;

use tempfile::TempDir;
use uuid::Uuid;
use vexmap_core::{
    Actor, Category, ClientName, CommandContext, Core, CoreError, CountryId, Module, Outcome, RegionId, RequestId, Role,
};

/// Keeps a SQLite file alive for the test's duration.
struct Backend {
    core: Core,
    _dir: Option<TempDir>,
}

async fn sqlite() -> Backend {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    Backend { core: Core::connect(&url).await.unwrap(), _dir: Some(dir) }
}

async fn postgres(url: &str) -> Backend {
    use sqlx::Connection;
    let mut connection = sqlx::PgConnection::connect(url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public").execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    Backend { core: Core::connect(url).await.unwrap(), _dir: None }
}

async fn on_every_backend<F, Fut>(test: F)
where
    F: Fn(Core) -> Fut,
    Fut: Future<Output = ()>,
{
    let backend = sqlite().await;
    test(backend.core.clone()).await;
    if let Ok(url) = std::env::var("VEXMAP_TEST_POSTGRES_URL") {
        let backend = postgres(&url).await;
        test(backend.core.clone()).await;
    }
}

fn system() -> Actor {
    Actor::System(ClientName::new("test").unwrap())
}

fn ctx(actor: Actor, request: &str) -> CommandContext {
    CommandContext { actor, region: RegionId::new("a").unwrap(), request_id: RequestId::new(request).unwrap() }
}

fn none() -> BTreeSet<Category> {
    BTreeSet::new()
}

async fn make(core: &Core, role: Role, categories: BTreeSet<Category>) -> Uuid {
    let id = Uuid::new_v4();
    core.set_role(&ctx(system(), &format!("make-{id}")), id, role, categories).await.unwrap();
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_and_its_journal_entry_land_together() {
    on_every_backend(|core| async move {
        let id = Uuid::new_v4();
        let outcome = core.record_player(&ctx(system(), "r1"), id, "Steve_1").await.unwrap();
        assert_eq!(outcome, Outcome::Applied { seq: 1 });
        assert_eq!(core.journal_head().await.unwrap(), 1);

        let player = core.player(id).await.unwrap().unwrap();
        assert_eq!((player.name.as_deref(), player.role), (Some("Steve_1"), Role::Player));

        let entries = core.journal(0, 10).await.unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!((entry.seq, entry.kind.as_str(), entry.region.as_str()), (1, "player.recorded", "a"));
        assert_eq!(entry.actor, "system:test");
        assert_eq!(entry.object_id, id.to_string());
        assert_eq!(entry.before, None);
        assert_eq!(entry.after.as_ref().unwrap()["name"], "Steve_1");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retrying_a_request_applies_it_once_even_with_different_data() {
    on_every_backend(|core| async move {
        let id = Uuid::new_v4();
        let first = core.record_player(&ctx(system(), "same"), id, "First").await.unwrap();
        let second = core.record_player(&ctx(system(), "same"), id, "Second").await.unwrap();
        assert_eq!(first, Outcome::Applied { seq: 1 });
        assert_eq!(second, Outcome::Replayed { seq: 1 });
        assert_eq!(core.player(id).await.unwrap().unwrap().name.as_deref(), Some("First"));
        assert_eq!(core.journal_head().await.unwrap(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_change_writes_nothing_and_leaves_no_gap() {
    on_every_backend(|core| async move {
        let id = Uuid::new_v4();
        core.record_player(&ctx(system(), "n1"), id, "Same").await.unwrap();
        assert_eq!(core.record_player(&ctx(system(), "n2"), id, "Same").await.unwrap(), Outcome::Unchanged);
        let next = core.record_player(&ctx(system(), "n3"), id, "Other").await.unwrap();
        assert_eq!(next, Outcome::Applied { seq: 2 });
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_command_leaves_no_trace() {
    on_every_backend(|core| async move {
        let player = make(&core, Role::Player, none()).await;
        let head = core.journal_head().await.unwrap();
        let result = core.set_role(&ctx(Actor::Player(player), "x"), player, Role::Admin, none()).await;
        assert!(matches!(result, Err(CoreError::Forbidden(_))));
        assert_eq!(core.journal_head().await.unwrap(), head);
        assert_eq!(core.player(player).await.unwrap().unwrap().role, Role::Player);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_player_has_no_powers() {
    on_every_backend(|core| async move {
        let stranger = Actor::Player(Uuid::new_v4());
        let result = core.set_feature(&ctx(stranger, "x"), Module::War, &RegionId::global(), true).await;
        assert!(matches!(result, Err(CoreError::Forbidden(_))));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_are_checked_against_the_database() {
    on_every_backend(|core| async move {
        let admin = make(&core, Role::Admin, none()).await;
        let owner = make(&core, Role::Owner, none()).await;
        let player = make(&core, Role::Player, none()).await;

        let wiki: BTreeSet<_> = [Category::Wiki].into();
        core.set_role(&ctx(Actor::Player(admin), "a1"), player, Role::Moderator, wiki.clone()).await.unwrap();
        let moderator = core.player(player).await.unwrap().unwrap();
        assert_eq!((moderator.role, moderator.categories), (Role::Moderator, wiki));

        let promote = core.set_role(&ctx(Actor::Player(admin), "a2"), player, Role::Admin, none()).await;
        assert!(matches!(promote, Err(CoreError::Forbidden(_))));
        let attack = core.set_role(&ctx(Actor::Player(admin), "a3"), owner, Role::Player, none()).await;
        assert!(matches!(attack, Err(CoreError::Forbidden(_))));
        core.set_role(&ctx(Actor::Player(owner), "o1"), player, Role::Admin, none()).await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_owner_stays() {
    on_every_backend(|core| async move {
        let owner = make(&core, Role::Owner, none()).await;
        let result = core.set_role(&ctx(system(), "l1"), owner, Role::Admin, none()).await;
        assert!(matches!(result, Err(CoreError::LastOwner)));

        let second = make(&core, Role::Owner, none()).await;
        core.set_role(&ctx(Actor::Player(second), "l2"), owner, Role::Admin, none()).await.unwrap();
        assert_eq!(core.player(owner).await.unwrap().unwrap().role, Role::Admin);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn moderators_and_only_moderators_have_categories() {
    on_every_backend(|core| async move {
        let id = Uuid::new_v4();
        let empty = core.set_role(&ctx(system(), "c1"), id, Role::Moderator, none()).await;
        assert!(matches!(empty, Err(CoreError::Invalid(_))));
        let extra = core.set_role(&ctx(system(), "c2"), id, Role::Trusted, [Category::Wiki].into()).await;
        assert!(matches!(extra, Err(CoreError::Invalid(_))));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_regions_flag_overrides_the_global_one_and_unset_means_off() {
    on_every_backend(|core| async move {
        let a = RegionId::new("a").unwrap();
        let b = RegionId::new("b").unwrap();
        assert!(core.enabled_modules(&a).await.unwrap().is_empty());

        core.set_feature(&ctx(system(), "f1"), Module::Tiles, &RegionId::global(), true).await.unwrap();
        core.set_feature(&ctx(system(), "f2"), Module::Tiles, &a, false).await.unwrap();
        core.set_feature(&ctx(system(), "f3"), Module::War, &a, true).await.unwrap();

        assert_eq!(core.enabled_modules(&a).await.unwrap(), [Module::War].into());
        assert_eq!(core.enabled_modules(&b).await.unwrap(), [Module::Tiles].into());

        let again = core.set_feature(&ctx(system(), "f4"), Module::War, &a, true).await.unwrap();
        assert_eq!(again, Outcome::Unchanged);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_produce_a_gapless_journal() {
    on_every_backend(|core| async move {
        let mut tasks = Vec::new();
        for task in 0..16 {
            let core = core.clone();
            tasks.push(tokio::spawn(async move {
                for step in 0..10 {
                    let name = format!("p{task}_{step}");
                    let request = ctx(system(), &name);
                    core.record_player(&request, Uuid::new_v4(), &name).await.unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let entries = core.journal(0, 1000).await.unwrap();
        assert_eq!(entries.len(), 160);
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.seq, index as i64 + 1);
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_pages_through_the_journal() {
    on_every_backend(|core| async move {
        for step in 0..5 {
            let name = format!("r{step}");
            core.record_player(&ctx(system(), &name), Uuid::new_v4(), &name).await.unwrap();
        }
        let page = core.journal(2, 2).await.unwrap();
        assert_eq!(page.iter().map(|entry| entry.seq).collect::<Vec<_>>(), [3, 4]);
        assert!(core.journal(5, 10).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn country_membership_is_journaled_and_guarded() {
    on_every_backend(|core| async move {
        let id = Uuid::new_v4();
        let north = CountryId::new("north").unwrap();
        let set = core.set_country(&ctx(system(), "c1"), id, Some(north.clone())).await.unwrap();
        assert!(matches!(set, Outcome::Applied { .. }));
        assert_eq!(core.player(id).await.unwrap().unwrap().country, Some(north.clone()));
        let again = core.set_country(&ctx(system(), "c2"), id, Some(north.clone())).await.unwrap();
        assert_eq!(again, Outcome::Unchanged);

        let moderator = make(&core, Role::Moderator, BTreeSet::from([Category::Wiki])).await;
        let refused = core.set_country(&ctx(Actor::Player(moderator), "c3"), id, None).await;
        assert!(matches!(refused, Err(CoreError::Forbidden(_))));

        core.record_player(&ctx(system(), "c4"), id, "Alex").await.unwrap();
        assert_eq!(core.player(id).await.unwrap().unwrap().country, Some(north), "a login keeps the country");

        let admin = make(&core, Role::Admin, none()).await;
        core.set_country(&ctx(Actor::Player(admin), "c5"), id, None).await.unwrap();
        assert_eq!(core.player(id).await.unwrap().unwrap().country, None);
        let last = core.journal(0, 100).await.unwrap().pop().unwrap();
        assert_eq!(
            (last.kind.as_str(), &last.after.unwrap()["country"]),
            ("player.country_changed", &serde_json::Value::Null)
        );
    })
    .await;
}

fn seq_of(outcome: Outcome) -> i64 {
    match outcome {
        Outcome::Applied { seq } | Outcome::Replayed { seq } => seq,
        Outcome::Unchanged => panic!("expected a change"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_route_network_is_drawn_closed_and_journaled() {
    use vexmap_core::RouteMode;
    on_every_backend(|core| async move {
        assert_eq!(core.route_network().await.unwrap().version, 0);
        let a = seq_of(core.add_route_node(&ctx(system(), "n1"), 0, 0, false).await.unwrap());
        let b = seq_of(core.add_route_node(&ctx(system(), "n2"), 100, 0, true).await.unwrap());
        let line = seq_of(core.add_route_line(&ctx(system(), "l1"), a, b, RouteMode::Walk, false).await.unwrap());

        let network = core.route_network().await.unwrap();
        assert_eq!((network.nodes.len(), network.lines.len(), network.version), (2, 1, line));
        assert!(network.nodes[1].transfer && !network.lines[0].closed);
        assert_eq!(core.route_network_version().await.unwrap(), line);

        // Refusals: same block, same pair and mode (either way round), unknown node, self loop, off the world.
        for result in [
            core.add_route_node(&ctx(system(), "n3"), 100, 0, false).await,
            core.add_route_node(&ctx(system(), "n4"), 31_000_000, 0, false).await,
            core.add_route_line(&ctx(system(), "l2"), b, a, RouteMode::Walk, true).await,
            core.add_route_line(&ctx(system(), "l3"), a, 999_999, RouteMode::Walk, false).await,
            core.add_route_line(&ctx(system(), "l4"), a, a, RouteMode::Walk, false).await,
        ] {
            assert!(matches!(result, Err(CoreError::Invalid(_))), "{result:?}");
        }
        // The same pair by another mode is a different line.
        assert!(core.add_route_line(&ctx(system(), "l5"), a, b, RouteMode::Train, false).await.is_ok());

        let closed = core.set_route_line_closed(&ctx(system(), "c1"), line, true).await.unwrap();
        let version = seq_of(closed);
        assert!(core.route_network().await.unwrap().lines[0].closed);
        assert_eq!(core.route_network_version().await.unwrap(), version);
        let again = core.set_route_line_closed(&ctx(system(), "c2"), line, true).await.unwrap();
        assert_eq!(again, Outcome::Unchanged);
        assert!(core.set_route_line_closed(&ctx(system(), "c3"), 12345, true).await.is_err());

        // A retried request id does not draw twice.
        let replay = core.add_route_node(&ctx(system(), "n1"), 5, 5, false).await.unwrap();
        assert_eq!(replay, Outcome::Replayed { seq: a });
        assert_eq!(core.route_network().await.unwrap().nodes.len(), 2);

        let moderator = make(&core, Role::Moderator, BTreeSet::from([Category::Places])).await;
        let refused = core.add_route_node(&ctx(Actor::Player(moderator), "n5"), 9, 9, false).await;
        assert!(matches!(refused, Err(CoreError::Forbidden(_))));
        let admin = make(&core, Role::Admin, none()).await;
        assert!(core.add_route_node(&ctx(Actor::Player(admin), "n6"), 9, 9, false).await.is_ok());

        let kinds: Vec<_> = core.journal(0, 100).await.unwrap().into_iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&"route.line_closed_changed".to_owned()));
        assert_eq!(kinds.iter().filter(|k| k.starts_with("route.")).count(), 6);
    })
    .await;
}

#[tokio::test]
async fn an_unsupported_database_is_refused() {
    assert!(matches!(Core::connect("mysql://x").await, Err(CoreError::Invalid(_))));
}

#[tokio::test]
async fn reopening_keeps_the_data() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("t.db").display()).replace('\\', "/");
    let id = Uuid::new_v4();
    {
        let core = Core::connect(&url).await.unwrap();
        core.record_player(&ctx(system(), "k"), id, "Kept").await.unwrap();
    }
    let core = Core::connect(&url).await.unwrap();
    assert_eq!(core.player(id).await.unwrap().unwrap().name.as_deref(), Some("Kept"));
    assert_eq!(core.journal_head().await.unwrap(), 1);
}
