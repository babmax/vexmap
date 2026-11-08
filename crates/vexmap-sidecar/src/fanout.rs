//! Decides who sees whom and turns the newest positions into ready frames (rule 8: the mod only forwards them).
//!
//! A channel is a set of viewers who all see the same markers, so each frame is built once however many read it. A large
//! channel is divided into streams per cell of the world: a viewer reads the cells around them and sees the rest as
//! clusters. A key frame carries the whole channel every 10 s, and each delta describes the present against that key frame. A delta can
//! therefore be lost or dropped under backpressure at no cost.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;
use vexmap_core::{Core, CoreError, CountryId, MAX_JOURNAL_PAGE, Role};
use vexmap_link::{Delivery, encode_deliveries};
use vexmap_proto::{ChannelEncoder, Cluster, MAX_S2C_FRAME, Marker, Message, encode};

use crate::feed::PositionStore;

/// The rate of map positions: 2 Hz, the rate the traffic budget of ADR "Бюджеты" assumes.
const STEP: Duration = Duration::from_millis(500);
/// Steps between key frames: 10 s.
const KEY_EVERY: u32 = 20;
/// Up to this many markers everyone in a channel sees all of them. Past it the channel is divided into cells.
const DETAIL_ALL_UP_TO: usize = 256;
/// A divided channel goes back only below this, so a channel at the limit does not flip every step.
const DETAIL_RESUMES_BELOW: usize = 192;
/// A cell of a divided channel: 1024 blocks, in 1/8 of a block. A viewer reads the 3x3 around them.
const CELL: i32 = 1024 * 8;
/// The coarse grid of clusters: 4096 blocks, in 1/8 of a block.
const CLUSTER_CELL: i32 = 4096 * 8;
/// Clusters change slowly: every 2 s is enough.
const CLUSTERS_EVERY: u32 = 4;

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

/// One part of a channel: all of it, or one cell of the world.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Area {
    Whole,
    Cell(String, i32, i32),
}

type StreamKey = (Channel, Area);

struct StreamState {
    id: u32,
    encoder: ChannelEncoder,
    key_frame: Vec<u8>,
    steps: u32,
}

#[derive(Default)]
struct ViewerState {
    streams: Vec<u32>,
    clustered: bool,
}

/// What each region must send, per step. Pure, so every visibility rule is testable without sockets.
#[derive(Default)]
pub struct Fanout {
    streams: HashMap<StreamKey, StreamState>,
    viewers: HashMap<u128, ViewerState>,
    divided: HashSet<Channel>,
    clusters: HashMap<Channel, Vec<Cluster>>,
    step_number: u32,
    next_stream: u32,
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

fn cell_of(marker: &Marker) -> Area {
    Area::Cell(marker.dimension.clone(), marker.x.div_euclid(CELL), marker.z.div_euclid(CELL))
}

/// Players summed up on a coarse grid, each group placed at its own centre.
fn clusters_of(markers: &[Marker]) -> Vec<Cluster> {
    let mut groups: BTreeMap<(&str, i32, i32), (i64, i64, u32)> = BTreeMap::new();
    for marker in markers {
        let cell = (marker.dimension.as_str(), marker.x.div_euclid(CLUSTER_CELL), marker.z.div_euclid(CLUSTER_CELL));
        let group = groups.entry(cell).or_default();
        group.0 += i64::from(marker.x);
        group.1 += i64::from(marker.z);
        group.2 += 1;
    }
    groups
        .into_iter()
        .map(|((dimension, _, _), (x, z, count))| Cluster {
            dimension: dimension.to_owned(),
            x: (x / i64::from(count)) as i32,
            z: (z / i64::from(count)) as i32,
            count: count.min(u32::from(u16::MAX)) as u16,
        })
        .collect()
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

    /// Whether this channel is divided now. Two limits, so a channel at the edge does not flip every step.
    fn update_division(&mut self, channel: &Channel, markers: usize) -> bool {
        if markers > DETAIL_ALL_UP_TO {
            self.divided.insert(channel.clone());
        } else if markers < DETAIL_RESUMES_BELOW {
            self.divided.remove(channel);
        }
        self.divided.contains(channel)
    }

    /// `online` is sorted by player and holds each player once.
    pub fn step(&mut self, online: &[Online], directory: &HashMap<u128, Viewer>) -> Routed {
        let number = self.step_number;
        self.step_number = number.wrapping_add(1);

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
        let present: HashSet<u128> = online.iter().map(|player| player.marker.uuid).collect();
        self.viewers.retain(|uuid, _| present.contains(uuid));

        // Who reads which stream, and what each stream holds.
        let mut readers: BTreeMap<StreamKey, Vec<&Online>> = BTreeMap::new();
        let mut contents: HashMap<StreamKey, Vec<Marker>> = HashMap::new();
        let mut wanted: HashMap<u128, Vec<StreamKey>> = HashMap::new();
        let mut channel_clusters: Vec<(&Channel, Vec<Cluster>)> = Vec::new();
        for (channel, viewers) in &audiences {
            let markers: Vec<Marker> = match channel {
                Channel::Everyone => online.iter().map(|player| player.marker.clone()).collect(),
                Channel::Country(country) => countries.remove(country).unwrap_or_default(),
            };
            if !self.update_division(channel, markers.len()) {
                let key = (channel.clone(), Area::Whole);
                readers.insert(key.clone(), viewers.clone());
                for viewer in viewers {
                    wanted.entry(viewer.marker.uuid).or_default().push(key.clone());
                }
                contents.insert(key, markers);
                continue;
            }
            channel_clusters.push((channel, clusters_of(&markers)));
            let mut cells: HashMap<Area, Vec<Marker>> = HashMap::new();
            for marker in markers {
                cells.entry(cell_of(&marker)).or_default().push(marker);
            }
            for viewer in viewers {
                let Area::Cell(dimension, cell_x, cell_z) = cell_of(&viewer.marker) else { continue };
                for x in cell_x - 1..=cell_x + 1 {
                    for z in cell_z - 1..=cell_z + 1 {
                        let area = Area::Cell(dimension.clone(), x, z);
                        if cells.contains_key(&area) {
                            let key = (channel.clone(), area);
                            readers.entry(key.clone()).or_default().push(viewer);
                            wanted.entry(viewer.marker.uuid).or_default().push(key);
                        }
                    }
                }
            }
            for (area, list) in cells {
                let key = (channel.clone(), area);
                if readers.contains_key(&key) {
                    contents.insert(key, list);
                }
            }
        }

        // Key frames and deltas. Viewers' old subscriptions are still in place, which tells who has just joined.
        let mut frames = Routed::new();
        let mut streams = HashMap::with_capacity(readers.len());
        let mut ids: HashMap<&StreamKey, u32> = HashMap::with_capacity(readers.len());
        for (key, viewers) in &readers {
            let markers = contents.remove(key).unwrap_or_default();
            if let Some(state) = self.advance(key, viewers, markers, &mut frames) {
                ids.insert(key, state.id);
                streams.insert(key.clone(), state);
            }
        }
        // A stream nobody reads costs nothing, and comes back with a key frame.
        self.streams = streams;

        let mut out = Routed::new();
        self.send_stream_lists(online, &wanted, &ids, number, &mut out);
        self.send_clusters(online, &audiences, channel_clusters, number, &mut out);
        for (region, deliveries) in frames {
            out.entry(region).or_default().extend(deliveries);
        }
        out
    }

    /// Goes first, so a client never drops a stream it is about to receive. Repeated with the key frames, because a
    /// lost list would otherwise leave markers on the client for good.
    fn send_stream_lists(
        &mut self,
        online: &[Online],
        wanted: &HashMap<u128, Vec<StreamKey>>,
        ids: &HashMap<&StreamKey, u32>,
        number: u32,
        out: &mut Routed,
    ) {
        let repeat = number.is_multiple_of(KEY_EVERY + 1);
        let mut by_list: BTreeMap<Vec<u32>, Vec<&Online>> = BTreeMap::new();
        for player in online {
            let uuid = player.marker.uuid;
            let mut list: Vec<u32> = wanted
                .get(&uuid)
                .map_or(Vec::new(), |keys| keys.iter().filter_map(|key| ids.get(key).copied()).collect());
            list.sort_unstable();
            let known = self.viewers.get(&uuid);
            if known.map_or(!list.is_empty(), |state| state.streams != list) || (repeat && !list.is_empty()) {
                by_list.entry(list.clone()).or_default().push(player);
            }
            if !list.is_empty() || known.is_some_and(|state| state.clustered) {
                self.viewers.entry(uuid).or_default().streams = list;
            } else {
                self.viewers.remove(&uuid);
            }
        }
        for (list, viewers) in by_list {
            if let Some(frame) = self.frame(&Message::PositionStreams(list)) {
                route(out, &viewers, &frame);
            }
        }
    }

    fn send_clusters(
        &mut self,
        online: &[Online],
        audiences: &BTreeMap<Channel, Vec<&Online>>,
        channel_clusters: Vec<(&Channel, Vec<Cluster>)>,
        number: u32,
        out: &mut Routed,
    ) {
        let mut in_divided: HashSet<u128> = HashSet::new();
        let refresh = number.is_multiple_of(CLUSTERS_EVERY);
        for (channel, clusters) in channel_clusters {
            let Some(viewers) = audiences.get(channel) else { continue };
            let changed = self.clusters.get(channel).is_none_or(|known| refresh && *known != clusters);
            if changed {
                self.clusters.insert(channel.clone(), clusters);
            }
            let Some(current) = self.clusters.get(channel).cloned() else { continue };
            let is_new =
                |viewer: &&&Online| !self.viewers.get(&viewer.marker.uuid).is_some_and(|state| state.clustered);
            let addressees: Vec<&Online> = viewers.iter().filter(|viewer| changed || is_new(viewer)).copied().collect();
            if !addressees.is_empty()
                && let Some(frame) = self.frame(&Message::PositionClusters(current))
            {
                route(out, &addressees, &frame);
            }
            for viewer in viewers {
                in_divided.insert(viewer.marker.uuid);
                self.viewers.entry(viewer.marker.uuid).or_default().clustered = true;
            }
        }
        self.clusters.retain(|channel, _| self.divided.contains(channel) && audiences.contains_key(channel));

        let finished: Vec<&Online> = online
            .iter()
            .filter(|player| {
                !in_divided.contains(&player.marker.uuid)
                    && self.viewers.get(&player.marker.uuid).is_some_and(|state| state.clustered)
            })
            .collect();
        if !finished.is_empty()
            && let Some(frame) = self.frame(&Message::PositionClusters(Vec::new()))
        {
            route(out, &finished, &frame);
        }
        for viewer in finished {
            if let Some(state) = self.viewers.get_mut(&viewer.marker.uuid) {
                state.clustered = false;
            }
        }
    }

    fn advance(
        &mut self,
        key: &StreamKey,
        viewers: &[&Online],
        markers: Vec<Marker>,
        out: &mut Routed,
    ) -> Option<StreamState> {
        match self.streams.remove(key) {
            Some(state) if state.steps < KEY_EVERY => self.send_delta(state, viewers, markers, out),
            previous => self.send_key(previous.map(|state| state.id), viewers, markers, out),
        }
    }

    /// A key frame on schedule keeps its stream, and a new stream takes a new number.
    fn send_key(
        &mut self,
        id: Option<u32>,
        viewers: &[&Online],
        markers: Vec<Marker>,
        out: &mut Routed,
    ) -> Option<StreamState> {
        let id = id.unwrap_or_else(|| {
            self.next_stream = self.next_stream.wrapping_add(1);
            self.next_stream
        });
        let key = self.next_key();
        let mut encoder = ChannelEncoder::default();
        let key_frame = self.frame(&Message::PositionKey(encoder.key_frame(id, key, markers)))?;
        route(out, viewers, &key_frame);
        Some(StreamState { id, encoder, key_frame, steps: 0 })
    }

    fn send_delta(
        &mut self,
        mut state: StreamState,
        viewers: &[&Online],
        markers: Vec<Marker>,
        out: &mut Routed,
    ) -> Option<StreamState> {
        state.steps += 1;
        let (delta, changed) = state.encoder.delta(markers);
        let message = Message::PositionDelta(delta.clone());
        let frame = self.frame(&message)?;
        let (joined, staying): (Vec<&Online>, Vec<&Online>) = viewers.iter().copied().partition(|viewer| {
            self.viewers.get(&viewer.marker.uuid).is_none_or(|known| !known.streams.contains(&state.id))
        });
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
    use vexmap_proto::{Feed, Limits, PositionTables, Reassembler};

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
        tables: HashMap<u128, PositionTables>,
        bytes: HashMap<u128, usize>,
    }

    impl Clients {
        fn apply(&mut self, routed: &Routed) {
            for delivery in routed.values().flatten() {
                for viewer in &delivery.viewers {
                    let mut reassembler = Reassembler::new(Limits::default());
                    let Feed::Complete { kind, body } = reassembler.feed(&delivery.frame, 0) else { panic!() };
                    *self.bytes.entry(*viewer).or_default() += delivery.frame.len();
                    self.tables.entry(*viewer).or_default().apply(Message::decode(kind, &body).unwrap());
                }
            }
        }

        fn sees(&self, viewer: u128) -> Vec<u128> {
            let mut seen: Vec<u128> =
                self.tables.get(&viewer).map_or(Vec::new(), |table| table.markers().map(|m| m.uuid).collect());
            seen.sort_unstable();
            seen
        }

        fn x_of(&self, viewer: u128, target: u128) -> Option<i32> {
            self.tables.get(&viewer)?.markers().find(|m| m.uuid == target).map(|m| m.x)
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
        // The stream list is repeated in case the earlier one was lost, together with the key frame.
        assert_eq!(fanout.step(&online, &dir)["a"].len(), 2);
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

    /// A country far over the limit, spread over the world: 25 members per cell.
    fn crowd(step: i32) -> Vec<Online> {
        (0..5000).map(|uuid| player(uuid, "a", uuid as i32 * 320 + step * 16)).collect()
    }

    fn crowd_directory() -> HashMap<u128, Viewer> {
        (0..5000).map(|uuid| (uuid, member("north"))).collect()
    }

    #[test]
    fn a_large_channel_shows_the_neighbourhood_and_sums_up_the_rest() {
        let dir = crowd_directory();
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&crowd(0), &dir));

        let seen = clients.sees(2500);
        assert!(seen.contains(&2500) && seen.contains(&2510));
        assert!(!seen.contains(&0), "a member 100 km away is not a marker");
        assert!(seen.len() < 200, "{}", seen.len());
        let table = &clients.tables[&2500];
        let total: u32 = table.clusters().iter().map(|cluster| u32::from(cluster.count)).sum();
        assert_eq!(total, 5000, "everyone is still accounted for");
    }

    #[test]
    fn walking_into_the_next_cell_changes_what_is_shown() {
        let mut members = vec![(1, member("north"))];
        let mut online = vec![player(1, "a", 0)];
        for uuid in 2..400u128 {
            members.push((uuid, member("north")));
            online.push(player(uuid, "a", 8192 * 10 + uuid as i32));
        }
        let dir: HashMap<u128, Viewer> = members.into_iter().collect();
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&online, &dir));
        assert_eq!(clients.sees(1), [1]);

        online[0] = player(1, "a", 8192 * 9);
        clients.apply(&fanout.step(&online, &dir));
        clients.apply(&fanout.step(&online, &dir));
        assert_eq!(clients.sees(1).len(), 399);

        online[0] = player(1, "a", 0);
        clients.apply(&fanout.step(&online, &dir));
        clients.apply(&fanout.step(&online, &dir));
        assert_eq!(clients.sees(1), [1], "markers left behind are gone");
    }

    #[test]
    fn a_channel_that_shrinks_goes_back_to_showing_everyone() {
        let mut dir = HashMap::new();
        let many: Vec<Online> = (0..300).map(|uuid| player(uuid, "a", uuid as i32 * 8192 * 4)).collect();
        for uuid in 0..300 {
            dir.insert(uuid, member("north"));
        }
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        clients.apply(&fanout.step(&many, &dir));
        assert!(clients.sees(0).len() < 10);

        // Between the two limits it stays divided, so a channel at the limit does not flip every step.
        let middle = &many[..DETAIL_RESUMES_BELOW + 10];
        clients.apply(&fanout.step(middle, &dir));
        assert!(clients.sees(0).len() < 10);

        let few = &many[..DETAIL_RESUMES_BELOW - 1];
        clients.apply(&fanout.step(few, &dir));
        clients.apply(&fanout.step(few, &dir));
        assert_eq!(clients.sees(0).len(), DETAIL_RESUMES_BELOW - 1);
        assert!(clients.tables[&0].clusters().is_empty(), "clusters go away with the division");
    }

    #[test]
    fn another_country_never_shares_a_cell() {
        let mut dir = crowd_directory();
        for uuid in (0..5000).step_by(2) {
            dir.insert(uuid, member("south"));
        }
        let mut clients = Clients::default();
        clients.apply(&Fanout::default().step(&crowd(0), &dir));
        assert!(clients.sees(2500).iter().all(|uuid| uuid % 2 == 0));
        assert!(clients.sees(2501).iter().all(|uuid| uuid % 2 == 1));
    }

    /// The budget of ADR "Бюджеты": 16 KB/s on average. Every member of a 5000-strong channel walks all the time.
    #[test]
    fn the_largest_channel_stays_inside_the_traffic_budget() {
        let dir = crowd_directory();
        let mut fanout = Fanout::default();
        let mut clients = Clients::default();
        let steps = KEY_EVERY as i32 * 3;
        for step in 0..steps {
            clients.apply(&fanout.step(&crowd(step), &dir));
        }
        let per_second = clients.bytes[&2500] as f64 / (f64::from(steps) * STEP.as_secs_f64());
        println!("traffic to one viewer: {:.0} B/s", per_second);
        assert!(per_second < 16_000.0, "{per_second:.0} B/s");
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
