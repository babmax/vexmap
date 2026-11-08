//! Decides who sees whom and turns the newest positions into ready frames (rule 8: the mod only forwards them).
//!
//! A channel is a set of viewers who all see the same markers, so each frame is built once however many read it. A key
//! frame carries the whole channel every 10 s, and each delta describes the present against that key frame. A delta can
//! therefore be lost or dropped under backpressure at no cost.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;
use vexmap_core::{Core, CoreError, CountryId, MAX_JOURNAL_PAGE, Role};
use vexmap_link::{Delivery, encode_deliveries};
use vexmap_proto::{ChannelEncoder, MAX_S2C_FRAME, Marker, Message, PositionKey, encode};

use crate::feed::PositionStore;

/// The rate of map positions: 2 Hz, the rate the traffic budget of ADR "Бюджеты" assumes.
const STEP: Duration = Duration::from_millis(500);
/// Steps between key frames: 10 s.
const KEY_EVERY: u32 = 20;

/// What decides which markers a player may see. Always read from the database, never from the game or the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Viewer {
    pub role: Role,
    pub country: Option<CountryId>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channel {
    /// Administrators and moderators (assumption 1).
    Everyone,
    Country(CountryId),
}

/// The channel a player reads. `None` is a player without a country: they see only themselves, and the client already
/// knows where it is, so nothing is sent.
pub fn channel_of(viewer: &Viewer) -> Option<Channel> {
    if viewer.role >= Role::Moderator {
        return Some(Channel::Everyone);
    }
    viewer.country.clone().map(Channel::Country)
}

/// A player on some server, with their newest position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Online {
    pub region: String,
    pub marker: Marker,
}

struct ChannelState {
    encoder: ChannelEncoder,
    key_frame: Vec<u8>,
    steps: u32,
}

/// What each region must send, per step. Pure, so every visibility rule is testable without sockets.
#[derive(Default)]
pub struct Fanout {
    channels: HashMap<Channel, ChannelState>,
    subscribed: HashMap<u128, Channel>,
    next_key: u32,
    next_message: u32,
}

type Routed = HashMap<String, Vec<Delivery>>;

fn route(out: &mut Routed, viewers: &[&Online], frame: &[u8]) {
    let mut by_region: BTreeMap<&str, Vec<u128>> = BTreeMap::new();
    for viewer in viewers {
        by_region.entry(&viewer.region).or_default().push(viewer.marker.uuid);
    }
    for (region, viewers) in by_region {
        out.entry(region.to_owned()).or_default().push(Delivery { viewers, frame: frame.to_vec() });
    }
}

impl Fanout {
    fn next_key(&mut self) -> u32 {
        self.next_key = self.next_key.wrapping_add(1);
        self.next_key
    }

    /// One frame: `MAX_MARKERS` markers always fit one, so a frame is complete on its own.
    fn frame(&mut self, message: &Message) -> Option<Vec<u8>> {
        let number = self.next_message;
        self.next_message = self.next_message.wrapping_add(1);
        match encode(message.kind(), number, &message.body(), MAX_S2C_FRAME) {
            Ok(mut frames) if frames.len() == 1 => frames.pop(),
            _ => {
                tracing::warn!("fanout: a position frame does not fit one message");
                None
            }
        }
    }

    /// `online` is sorted by player and holds each player once.
    pub fn step(&mut self, online: &[Online], directory: &HashMap<u128, Viewer>) -> Routed {
        let mut out = Routed::new();
        let mut audiences: BTreeMap<Channel, Vec<&Online>> = BTreeMap::new();
        let mut countries: HashMap<&CountryId, Vec<Marker>> = HashMap::new();
        for player in online {
            let viewer = directory.get(&player.marker.uuid);
            if let Some(channel) = viewer.and_then(channel_of) {
                audiences.entry(channel).or_default().push(player);
            }
            if let Some(country) = viewer.and_then(|viewer| viewer.country.as_ref()) {
                countries.entry(country).or_default().push(player.marker.clone());
            }
        }

        // Whoever lost their channel but is still here must stop seeing what they saw.
        let present: HashMap<u128, &Online> = online.iter().map(|player| (player.marker.uuid, player)).collect();
        self.subscribed.retain(|uuid, _| present.contains_key(uuid));
        let cleared: Vec<&Online> = self
            .subscribed
            .keys()
            .filter(|uuid| directory.get(uuid).and_then(channel_of).is_none())
            .filter_map(|uuid| present.get(uuid).copied())
            .collect();
        if !cleared.is_empty() {
            let key = self.next_key();
            if let Some(frame) = self.frame(&Message::PositionKey(PositionKey { key, markers: Vec::new() })) {
                route(&mut out, &cleared, &frame);
            }
            for viewer in &cleared {
                self.subscribed.remove(&viewer.marker.uuid);
            }
        }

        let mut channels = HashMap::with_capacity(audiences.len());
        for (channel, viewers) in audiences {
            let markers = match &channel {
                Channel::Everyone => online.iter().map(|player| player.marker.clone()).collect(),
                Channel::Country(country) => countries.remove(country).unwrap_or_default(),
            };
            let state = self.advance(&channel, &viewers, markers, &mut out);
            for viewer in &viewers {
                self.subscribed.insert(viewer.marker.uuid, channel.clone());
            }
            if let Some(state) = state {
                channels.insert(channel, state);
            }
        }
        // A channel nobody reads costs nothing, and comes back with a key frame.
        self.channels = channels;
        out
    }

    fn advance(
        &mut self,
        channel: &Channel,
        viewers: &[&Online],
        markers: Vec<Marker>,
        out: &mut Routed,
    ) -> Option<ChannelState> {
        let state = self.channels.remove(channel);
        let Some(mut state) = state.filter(|state| state.steps < KEY_EVERY) else {
            let key = self.next_key();
            let mut encoder = ChannelEncoder::default();
            let key_frame = self.frame(&Message::PositionKey(encoder.key_frame(key, markers)))?;
            route(out, viewers, &key_frame);
            return Some(ChannelState { encoder, key_frame, steps: 0 });
        };
        state.steps += 1;
        let (delta, changed) = state.encoder.delta(markers);
        let message = Message::PositionDelta(delta.clone());
        let frame = self.frame(&message)?;
        let (joined, staying): (Vec<&Online>, Vec<&Online>) =
            viewers.iter().copied().partition(|viewer| self.subscribed.get(&viewer.marker.uuid) != Some(channel));
        if changed {
            route(out, &staying, &frame);
        }
        if !joined.is_empty() {
            route(out, &joined, &state.key_frame);
            route(out, &joined, &frame);
        }
        Some(state)
    }
}

/// Role and country of whoever is online, kept current from the journal so a change applies within one step.
pub struct Directory {
    viewers: HashMap<u128, Viewer>,
    cursor: i64,
}

impl Directory {
    pub async fn new(core: &Core) -> Result<Self, CoreError> {
        Ok(Directory { viewers: HashMap::new(), cursor: core.journal_head().await? })
    }

    pub fn viewers(&self) -> &HashMap<u128, Viewer> {
        &self.viewers
    }

    /// On an error the last known state stays: the database that failed is also what changes it.
    pub async fn refresh(&mut self, core: &Core, online: &[Online]) -> Result<(), CoreError> {
        let entries = core.journal(self.cursor, MAX_JOURNAL_PAGE).await?;
        for entry in &entries {
            self.cursor = entry.seq;
            if entry.object_type == "player"
                && let Ok(id) = entry.object_id.parse::<Uuid>()
            {
                self.viewers.remove(&id.as_u128());
            }
        }
        let present: HashSet<u128> = online.iter().map(|player| player.marker.uuid).collect();
        self.viewers.retain(|uuid, _| present.contains(uuid));
        for uuid in present {
            if self.viewers.contains_key(&uuid) {
                continue;
            }
            let player = core.player(Uuid::from_u128(uuid)).await?;
            let viewer = player.map_or(Viewer { role: Role::Player, country: None }, |player| Viewer {
                role: player.role,
                country: player.country,
            });
            self.viewers.insert(uuid, viewer);
        }
        Ok(())
    }
}

/// Runs for the life of the sidecar. One slow step only delays the next: the tick is never involved.
pub async fn run(core: Core, store: Arc<PositionStore>) {
    let mut directory = match Directory::new(&core).await {
        Ok(directory) => directory,
        Err(error) => {
            tracing::error!("fanout did not start: {error}");
            return;
        }
    };
    let mut fanout = Fanout::default();
    let mut interval = tokio::time::interval(STEP);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let online = store.online();
        if let Err(error) = directory.refresh(&core, &online).await {
            tracing::warn!("fanout: directory not refreshed: {error}");
        }
        for (region, deliveries) in fanout.step(&online, directory.viewers()) {
            let mut bytes = Vec::new();
            encode_deliveries(&deliveries, &mut bytes);
            store.deliver(&region, bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use vexmap_proto::{Feed, Limits, PositionTable, Reassembler};

    use super::*;

    fn country(name: &str) -> Option<CountryId> {
        Some(CountryId::new(name).unwrap())
    }

    fn player(uuid: u128, region: &str, x: i32) -> Online {
        let marker = Marker {
            uuid,
            name: format!("P{uuid}"),
            dimension: "minecraft:overworld".to_owned(),
            x,
            y: 0,
            z: 0,
            yaw: 0,
        };
        Online { region: region.to_owned(), marker }
    }

    fn member(name: &str) -> Viewer {
        Viewer { role: Role::Player, country: country(name) }
    }

    fn directory(entries: &[(u128, Viewer)]) -> HashMap<u128, Viewer> {
        entries.iter().cloned().collect()
    }

    /// Plays deliveries into one client per player and shows what each of them ends up seeing.
    #[derive(Default)]
    struct Clients {
        tables: HashMap<u128, PositionTable>,
    }

    impl Clients {
        fn apply(&mut self, routed: &Routed) {
            for delivery in routed.values().flatten() {
                for viewer in &delivery.viewers {
                    let mut reassembler = Reassembler::new(Limits::default());
                    let Feed::Complete { kind, body } = reassembler.feed(&delivery.frame, 0) else { panic!() };
                    let table = self.tables.entry(*viewer).or_default();
                    match Message::decode(kind, &body).unwrap() {
                        Message::PositionKey(frame) => table.apply_key(frame),
                        Message::PositionDelta(delta) => {
                            table.apply_delta(&delta);
                        }
                        other => panic!("{other:?}"),
                    }
                }
            }
        }

        fn sees(&self, viewer: u128) -> Vec<u128> {
            let mut seen: Vec<u128> =
                self.tables.get(&viewer).map_or(Vec::new(), |table| table.markers().iter().map(|m| m.uuid).collect());
            seen.sort_unstable();
            seen
        }

        fn x_of(&self, viewer: u128, target: u128) -> Option<i32> {
            self.tables.get(&viewer)?.markers().iter().find(|m| m.uuid == target).map(|m| m.x)
        }
    }

    #[test]
    fn roles_and_countries_pick_the_channel() {
        assert_eq!(channel_of(&Viewer { role: Role::Player, country: None }), None);
        assert_eq!(channel_of(&member("north")), Some(Channel::Country(CountryId::new("north").unwrap())));
        let trusted = Viewer { role: Role::Trusted, country: None };
        assert_eq!(channel_of(&trusted), None);
        for role in [Role::Moderator, Role::Admin, Role::Owner] {
            assert_eq!(channel_of(&Viewer { role, country: country("north") }), Some(Channel::Everyone));
        }
    }

    #[test]
    fn a_country_sees_only_itself_across_regions() {
        let online = [player(1, "a", 8), player(2, "b", 16), player(3, "a", 24), player(4, "a", 32), player(5, "b", 0)];
        let dir = directory(&[
            (1, member("north")),
            (2, member("north")),
            (3, member("south")),
            (4, Viewer { role: Role::Player, country: None }),
            (5, Viewer { role: Role::Admin, country: None }),
        ]);
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&online, &dir));

        assert_eq!(clients.sees(1), [1, 2], "members on both servers");
        assert_eq!(clients.sees(2), [1, 2]);
        assert_eq!(clients.sees(3), [3]);
        assert_eq!(clients.sees(4), [] as [u128; 0], "no country, nothing sent");
        assert_eq!(clients.sees(5), [1, 2, 3, 4, 5], "an admin sees everyone");
    }

    #[test]
    fn frames_go_only_to_the_region_of_each_viewer() {
        let online = [player(1, "a", 8), player(2, "b", 16)];
        let dir = directory(&[(1, member("north")), (2, member("north"))]);
        let routed = Fanout::default().step(&online, &dir);
        assert_eq!(routed["a"][0].viewers, [1]);
        assert_eq!(routed["b"][0].viewers, [2]);
        assert_eq!(routed["a"][0].frame, routed["b"][0].frame, "built once");
    }

    #[test]
    fn a_still_channel_sends_nothing_until_it_changes() {
        let dir = directory(&[(1, member("north"))]);
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&[player(1, "a", 8)], &dir));
        assert!(fanout.step(&[player(1, "a", 8)], &dir).is_empty());

        let moved = fanout.step(&[player(1, "a", 64)], &dir);
        assert_eq!(moved["a"].len(), 1);
        clients.apply(&moved);
        assert_eq!(clients.x_of(1, 1), Some(64));
    }

    #[test]
    fn a_key_frame_comes_on_schedule() {
        let dir = directory(&[(1, member("north"))]);
        let mut fanout = Fanout::default();
        let online = [player(1, "a", 8)];
        fanout.step(&online, &dir);
        for _ in 0..KEY_EVERY {
            assert!(fanout.step(&online, &dir).is_empty());
        }
        assert_eq!(fanout.step(&online, &dir)["a"].len(), 1);
    }

    #[test]
    fn a_late_joiner_gets_the_key_frame_and_the_latest_delta() {
        let mut dir = directory(&[(1, member("north"))]);
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&[player(1, "a", 8)], &dir));
        clients.apply(&fanout.step(&[player(1, "a", 64)], &dir));

        dir.insert(2, member("north"));
        let online = [player(1, "a", 64), player(2, "b", 0)];
        clients.apply(&fanout.step(&online, &dir));
        clients.apply(&fanout.step(&online, &dir));
        assert_eq!(clients.sees(2), [1, 2]);
        assert_eq!(clients.x_of(2, 1), Some(64));
    }

    #[test]
    fn leaving_a_country_clears_what_was_visible() {
        let mut dir = directory(&[(1, member("north")), (2, member("north"))]);
        let online = [player(1, "a", 8), player(2, "a", 16)];
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&online, &dir));
        assert_eq!(clients.sees(1), [1, 2]);

        dir.insert(1, Viewer { role: Role::Player, country: None });
        clients.apply(&fanout.step(&online, &dir));
        clients.apply(&fanout.step(&online, &dir));
        assert_eq!(clients.sees(1), [] as [u128; 0]);
        assert_eq!(clients.sees(2), [2], "and the others stop seeing them");
    }

    #[test]
    fn changing_country_replaces_the_markers_entirely() {
        let mut dir = directory(&[(1, member("north")), (2, member("south")), (3, member("north"))]);
        let online = [player(1, "a", 8), player(2, "a", 16), player(3, "a", 24)];
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&online, &dir));
        clients.apply(&fanout.step(&online, &dir));

        dir.insert(1, member("south"));
        clients.apply(&fanout.step(&online, &dir));
        clients.apply(&fanout.step(&online, &dir));
        assert_eq!(clients.sees(1), [1, 2]);
    }

    #[test]
    fn a_viewer_the_directory_does_not_know_gets_nothing() {
        let routed = Fanout::default().step(&[player(1, "a", 8)], &HashMap::new());
        assert!(routed.is_empty());
    }

    #[test]
    fn coming_back_starts_from_a_key_frame() {
        let dir = directory(&[(1, member("north")), (2, member("north"))]);
        let mut fanout = Fanout::default();
        let both = [player(1, "a", 8), player(2, "a", 16)];
        fanout.step(&both, &dir);
        fanout.step(&[player(2, "a", 16)], &dir);

        let mut clients = Clients::default();
        clients.apply(&fanout.step(&both, &dir));
        assert_eq!(clients.sees(1), [1, 2]);
    }
}
