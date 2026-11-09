//! Routing over the hand-drawn network (roads, rails, sea lanes). Pure `std`: it runs on the client over a graph the
//! server hands out, or on the sidecar for a request, with no change to the code.
//!
//! World coordinates are blocks, `x` east and `z` south. Costs are seconds. A route may change mode (walk, train, boat)
//! only at a node marked as a transfer, and every change costs `Speeds::transfer_penalty`.
//!
//! Off the network, walking is a straight line (a bearing): the world has no roads there, a straight line is what a
//! player would do, and the leg is marked `Direct` so the client can say "head 40 degrees" instead of inventing streets.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Walk,
    Boat,
    Train,
}

const MODES: [Mode; 3] = [Mode::Walk, Mode::Boat, Mode::Train];

impl Mode {
    fn index(self) -> usize {
        self as usize
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Walk => "walk",
            Mode::Boat => "boat",
            Mode::Train => "train",
        }
    }

    pub fn parse(value: &str) -> Option<Mode> {
        MODES.into_iter().find(|mode| mode.as_str() == value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub z: f64,
}

impl Point {
    pub fn distance(self, other: Point) -> f64 {
        (self.x - other.x).hypot(self.z - other.z)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EdgeId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Speeds {
    /// Blocks per second.
    pub walk: f64,
    pub boat: f64,
    pub train: f64,
    /// Seconds lost changing mode at a station.
    pub transfer_penalty: f64,
}

impl Default for Speeds {
    fn default() -> Self {
        Speeds { walk: 4.3, boat: 8.0, train: 8.0, transfer_penalty: 30.0 }
    }
}

impl Speeds {
    pub fn of(&self, mode: Mode) -> f64 {
        match mode {
            Mode::Walk => self.walk,
            Mode::Boat => self.boat,
            Mode::Train => self.train,
        }
    }

    fn fastest(&self) -> f64 {
        self.walk.max(self.boat).max(self.train)
    }

    fn valid(&self) -> bool {
        [self.walk, self.boat, self.train].iter().all(|v| v.is_finite() && *v > 0.0)
            && self.transfer_penalty.is_finite()
            && self.transfer_penalty >= 0.0
    }
}

/// The player's real speed per mode, as a moving average: the estimate follows the player but one stall cannot ruin it.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpeedTracker {
    average: [Option<f64>; 3],
}

impl SpeedTracker {
    const ALPHA: f64 = 0.2;

    /// A sample: `blocks` covered in `seconds` while moving in `mode`. Nonsense and stalls are ignored.
    pub fn record(&mut self, mode: Mode, blocks: f64, seconds: f64) {
        if !(blocks.is_finite() && seconds.is_finite()) || blocks <= 0.0 || seconds <= 0.0 {
            return;
        }
        let speed = blocks / seconds;
        let slot = &mut self.average[mode.index()];
        *slot = Some(match *slot {
            Some(previous) => previous + Self::ALPHA * (speed - previous),
            None => speed,
        });
    }

    /// The defaults, with each mode replaced by what this player has shown. Clamped to 0.5x..2x of the default so one
    /// odd sample (an elytra, a lag spike) cannot make the estimate absurd.
    pub fn speeds(&self, base: Speeds) -> Speeds {
        let pick = |mode: Mode| match self.average[mode.index()] {
            Some(value) => value.clamp(base.of(mode) * 0.5, base.of(mode) * 2.0),
            None => base.of(mode),
        };
        Speeds { walk: pick(Mode::Walk), boat: pick(Mode::Boat), train: pick(Mode::Train), ..base }
    }
}

#[derive(Clone, Copy, Debug)]
struct Node {
    at: Point,
    transfer: bool,
}

#[derive(Clone, Copy, Debug)]
struct Edge {
    to: NodeId,
    mode: Mode,
    length: f64,
    closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    UnknownNode,
    SelfLoop,
}

#[derive(Default)]
pub struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    out: Vec<Vec<u32>>,
}

impl Graph {
    pub fn new() -> Self {
        Graph::default()
    }

    pub fn add_node(&mut self, x: f64, z: f64, transfer: bool) -> NodeId {
        self.nodes.push(Node { at: Point { x, z }, transfer });
        self.out.push(Vec::new());
        NodeId(self.nodes.len() as u32 - 1)
    }

    /// A drawn line between two nodes. `one_way` makes it usable from `a` to `b` only. Returns the edges created.
    pub fn add_line(&mut self, a: NodeId, b: NodeId, mode: Mode, one_way: bool) -> Result<Vec<EdgeId>, GraphError> {
        let pa = self.nodes.get(a.0 as usize).ok_or(GraphError::UnknownNode)?.at;
        let pb = self.nodes.get(b.0 as usize).ok_or(GraphError::UnknownNode)?.at;
        if a == b {
            return Err(GraphError::SelfLoop);
        }
        let length = pa.distance(pb);
        let mut made = vec![self.push_edge(a, b, mode, length)];
        if !one_way {
            made.push(self.push_edge(b, a, mode, length));
        }
        Ok(made)
    }

    fn push_edge(&mut self, from: NodeId, to: NodeId, mode: Mode, length: f64) -> EdgeId {
        let id = self.edges.len() as u32;
        self.edges.push(Edge { to, mode, length, closed: false });
        self.out[from.0 as usize].push(id);
        EdgeId(id)
    }

    /// Closes or reopens a section (works, war, a closed border). A closed edge is never used.
    pub fn set_closed(&mut self, edge: EdgeId, closed: bool) {
        if let Some(edge) = self.edges.get_mut(edge.0 as usize) {
            edge.closed = closed;
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn position(&self, node: NodeId) -> Option<Point> {
        self.nodes.get(node.0 as usize).map(|n| n.at)
    }

    /// The nearest node to `at` that has an open edge of `mode`, within `max` blocks.
    pub fn nearest(&self, at: Point, mode: Mode, max: f64) -> Option<NodeId> {
        let mut best: Option<(NodeId, f64)> = None;
        for (index, node) in self.nodes.iter().enumerate() {
            let d = node.at.distance(at);
            if d > max || best.is_some_and(|(_, b)| d >= b) {
                continue;
            }
            let usable = self.out[index].iter().any(|e| {
                let e = &self.edges[*e as usize];
                !e.closed && e.mode == mode
            });
            if usable {
                best = Some((NodeId(index as u32), d));
            }
        }
        best.map(|(n, _)| n)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegKind {
    Network,
    /// Off the network: a straight line by bearing.
    Direct,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Leg {
    pub mode: Mode,
    pub kind: LegKind,
    pub points: Vec<Point>,
    pub length: f64,
    pub seconds: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub legs: Vec<Leg>,
    pub length: f64,
    pub seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    BadSpeeds,
    NoStops,
    TooManyStops,
    /// Stop `n` (0-based) cannot be reached from the previous one.
    Unreachable(usize),
}

pub const MAX_STOPS: usize = 16;
/// How far a start or end may be from the network before the walk there is a straight line.
pub const SNAP_BLOCKS: f64 = 64.0;

#[derive(Clone, Copy, PartialEq)]
struct Item {
    f: f64,
    g: f64,
    node: u32,
    mode: u8,
}
impl Eq for Item {}
impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.total_cmp(&self.f)
    }
}
impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Graph {
    /// A* over (node, mode). Mode changes only at transfer nodes. Returns the node path with the mode at each step.
    fn search(&self, from: NodeId, to: NodeId, allowed: &[Mode], speeds: &Speeds) -> Option<Vec<(NodeId, Mode)>> {
        let goal = self.nodes[to.0 as usize].at;
        let h = |n: u32| self.nodes[n as usize].at.distance(goal) / speeds.fastest();
        let states = self.nodes.len() * MODES.len();
        let mut best = vec![f64::INFINITY; states];
        let mut came: Vec<Option<(u32, u8)>> = vec![None; states];
        let slot = |n: u32, m: u8| n as usize * MODES.len() + m as usize;
        let mut heap = BinaryHeap::new();
        for mode in allowed {
            let m = mode.index() as u8;
            best[slot(from.0, m)] = 0.0;
            heap.push(Item { f: h(from.0), g: 0.0, node: from.0, mode: m });
        }
        while let Some(Item { g, node, mode, .. }) = heap.pop() {
            if g > best[slot(node, mode)] {
                continue;
            }
            if node == to.0 {
                let mut path = vec![(NodeId(node), MODES[mode as usize])];
                let mut at = (node, mode);
                while let Some(prev) = came[slot(at.0, at.1)] {
                    path.push((NodeId(prev.0), MODES[prev.1 as usize]));
                    at = prev;
                }
                path.reverse();
                return Some(path);
            }
            let mut relax = |next: u32, next_mode: u8, cost: f64, heap: &mut BinaryHeap<Item>| {
                let ng = g + cost;
                let s = slot(next, next_mode);
                if ng < best[s] {
                    best[s] = ng;
                    came[s] = Some((node, mode));
                    heap.push(Item { f: ng + h(next), g: ng, node: next, mode: next_mode });
                }
            };
            for e in &self.out[node as usize] {
                let e = &self.edges[*e as usize];
                if e.closed || e.mode.index() as u8 != mode {
                    continue;
                }
                relax(e.to.0, mode, e.length / speeds.of(e.mode), &mut heap);
            }
            if self.nodes[node as usize].transfer {
                for other in allowed.iter().filter(|m| m.index() as u8 != mode) {
                    relax(node, other.index() as u8, speeds.transfer_penalty, &mut heap);
                }
            }
        }
        None
    }

    /// A route through `stops` in order, using only `allowed` modes. Each stop snaps to the network within
    /// `SNAP_BLOCKS`; farther away the walk to it is a straight `Direct` leg.
    pub fn route(&self, stops: &[Point], allowed: &[Mode], speeds: &Speeds) -> Result<Route, RouteError> {
        if !speeds.valid() {
            return Err(RouteError::BadSpeeds);
        }
        if stops.len() < 2 {
            return Err(RouteError::NoStops);
        }
        if stops.len() > MAX_STOPS {
            return Err(RouteError::TooManyStops);
        }
        let mut legs = Vec::new();
        for (index, pair) in stops.windows(2).enumerate() {
            self.route_pair(pair[0], pair[1], allowed, speeds, &mut legs).ok_or(RouteError::Unreachable(index + 1))?;
        }
        let length = legs.iter().map(|l| l.length).sum();
        let seconds = legs.iter().map(|l| l.seconds).sum();
        Ok(Route { legs, length, seconds })
    }

    fn route_pair(&self, a: Point, b: Point, allowed: &[Mode], speeds: &Speeds, legs: &mut Vec<Leg>) -> Option<()> {
        let walk = allowed.contains(&Mode::Walk);
        let direct = |p: Point, q: Point| Leg {
            mode: Mode::Walk,
            kind: LegKind::Direct,
            points: vec![p, q],
            length: p.distance(q),
            seconds: p.distance(q) / speeds.walk,
        };
        let (Some(start), Some(end)) = (self.snap(a, allowed), self.snap(b, allowed)) else {
            // Nothing near one end: walking straight is the only honest answer, and only if walking is allowed.
            return walk.then(|| legs.push(direct(a, b)));
        };
        let path = self.search(start, end, allowed, speeds)?;
        let from_at = self.nodes[start.0 as usize].at;
        if from_at.distance(a) > 0.5 && walk {
            legs.push(direct(a, from_at));
        }
        let mut current: Option<Leg> = None;
        for window in path.windows(2) {
            let ((n0, m0), (n1, _)) = (window[0], window[1]);
            if n0 == n1 {
                // A transfer: close the leg, the next begins at the same node with the new mode.
                legs.extend(current.take());
                if let Some(last) = legs.last_mut() {
                    last.seconds += speeds.transfer_penalty;
                }
                continue;
            }
            let p0 = self.nodes[n0.0 as usize].at;
            let p1 = self.nodes[n1.0 as usize].at;
            let leg = current.get_or_insert_with(|| Leg {
                mode: m0,
                kind: LegKind::Network,
                points: vec![p0],
                length: 0.0,
                seconds: 0.0,
            });
            let d = p0.distance(p1);
            leg.points.push(p1);
            leg.length += d;
            leg.seconds += d / speeds.of(m0);
        }
        legs.extend(current);
        let to_at = self.nodes[end.0 as usize].at;
        if to_at.distance(b) > 0.5 && walk {
            legs.push(direct(to_at, b));
        }
        Some(())
    }

    fn snap(&self, at: Point, allowed: &[Mode]) -> Option<NodeId> {
        allowed.iter().filter_map(|m| self.nearest(at, *m, SNAP_BLOCKS)).min_by(|x, y| {
            self.nodes[x.0 as usize].at.distance(at).total_cmp(&self.nodes[y.0 as usize].at.distance(at))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Straight,
    SlightLeft,
    Left,
    SharpLeft,
    SlightRight,
    Right,
    SharpRight,
    UTurn,
}

impl Turn {
    pub fn as_str(self) -> &'static str {
        match self {
            Turn::Straight => "straight",
            Turn::SlightLeft => "slight_left",
            Turn::Left => "left",
            Turn::SharpLeft => "sharp_left",
            Turn::SlightRight => "slight_right",
            Turn::Right => "right",
            Turn::SharpRight => "sharp_right",
            Turn::UTurn => "u_turn",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hint {
    pub at: Point,
    pub turn: Turn,
    /// Blocks from the previous hint (or the start).
    pub after: f64,
}

/// Heading in degrees: 0 north (`-z`), 90 east.
pub fn heading(from: Point, to: Point) -> f64 {
    (to.x - from.x).atan2(-(to.z - from.z)).to_degrees().rem_euclid(360.0)
}

/// `angle`: -180..180, positive is a right turn.
fn classify(angle: f64) -> Turn {
    let right = angle > 0.0;
    match angle.abs() {
        a if a < 20.0 => Turn::Straight,
        a if a < 50.0 => {
            if right {
                Turn::SlightRight
            } else {
                Turn::SlightLeft
            }
        }
        a if a < 120.0 => {
            if right {
                Turn::Right
            } else {
                Turn::Left
            }
        }
        a if a < 165.0 => {
            if right {
                Turn::SharpRight
            } else {
                Turn::SharpLeft
            }
        }
        _ => Turn::UTurn,
    }
}

impl Route {
    /// Every point of the route in order, without repeating a leg's join.
    pub fn polyline(&self) -> Vec<Point> {
        let mut out: Vec<Point> = Vec::new();
        for leg in &self.legs {
            for p in &leg.points {
                if out.last().is_none_or(|last| last.distance(*p) > 1e-9) {
                    out.push(*p);
                }
            }
        }
        out
    }

    /// Turn instructions: one per bend of 20 degrees or more.
    pub fn hints(&self) -> Vec<Hint> {
        let line = self.polyline();
        let mut hints = Vec::new();
        let mut since = 0.0;
        for w in line.windows(3) {
            since += w[0].distance(w[1]);
            let delta = (heading(w[1], w[2]) - heading(w[0], w[1]) + 540.0).rem_euclid(360.0) - 180.0;
            let turn = classify(delta);
            if turn != Turn::Straight {
                hints.push(Hint { at: w[1], turn, after: since });
                since = 0.0;
            }
        }
        hints
    }

    /// Where `at` is against the route: the distance from the line and how far along it the closest point lies.
    pub fn locate(&self, at: Point) -> (f64, f64) {
        let line = self.polyline();
        let mut best = (f64::INFINITY, 0.0);
        let mut walked = 0.0;
        for w in line.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (dx, dz) = (b.x - a.x, b.z - a.z);
            let len2 = dx * dx + dz * dz;
            let t = if len2 == 0.0 { 0.0 } else { (((at.x - a.x) * dx + (at.z - a.z) * dz) / len2).clamp(0.0, 1.0) };
            let nearest = Point { x: a.x + t * dx, z: a.z + t * dz };
            let d = nearest.distance(at);
            if d < best.0 {
                best = (d, walked + t * len2.sqrt());
            }
            walked += len2.sqrt();
        }
        best
    }
}

/// Follows a player along a route and says when to offer a new one. A single noisy sample never triggers it: the
/// player must stay off the line for `LIMIT` samples in a row. The new route is built only after the player confirms.
#[derive(Clone, Copy, Debug, Default)]
pub struct Deviation {
    off: u8,
}

impl Deviation {
    pub const DISTANCE: f64 = 24.0;
    pub const LIMIT: u8 = 3;

    /// `true` when the player should be offered a reroute.
    pub fn sample(&mut self, route: &Route, at: Point) -> bool {
        if route.locate(at).0 > Self::DISTANCE {
            self.off = self.off.saturating_add(1);
        } else {
            self.off = 0;
        }
        self.off >= Self::LIMIT
    }

    /// After the player declines or a new route is built.
    pub fn reset(&mut self) {
        self.off = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, z: f64) -> Point {
        Point { x, z }
    }

    /// A road along z=0 (0..300), a rail from (100,0) up to (100,-400) and over to (300,-400), then a road back down
    /// to (300,0) through (300,-100).
    fn town() -> Graph {
        let mut g = Graph::new();
        let a = g.add_node(0.0, 0.0, false);
        let b = g.add_node(100.0, 0.0, true);
        let c = g.add_node(300.0, 0.0, false);
        let d = g.add_node(100.0, -400.0, false);
        let e = g.add_node(300.0, -400.0, true);
        let f = g.add_node(300.0, -100.0, false);
        g.add_line(a, b, Mode::Walk, false).unwrap(); // edges 0, 1
        g.add_line(b, c, Mode::Walk, false).unwrap(); // 2, 3
        g.add_line(b, d, Mode::Train, false).unwrap();
        g.add_line(d, e, Mode::Train, false).unwrap();
        g.add_line(e, f, Mode::Walk, false).unwrap();
        g.add_line(f, c, Mode::Walk, false).unwrap();
        g
    }

    const ALL: [Mode; 3] = [Mode::Walk, Mode::Boat, Mode::Train];

    #[test]
    fn mode_names_round_trip() {
        for mode in ALL {
            assert_eq!(Mode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(Mode::parse("plane"), None);
    }

    #[test]
    fn walks_a_straight_road() {
        let g = town();
        let r = g.route(&[p(0.0, 0.0), p(300.0, 0.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        assert_eq!(r.legs.len(), 1);
        assert!((r.length - 300.0).abs() < 1e-9);
        assert!((r.seconds - 300.0 / 4.3).abs() < 1e-6);
    }

    #[test]
    fn combines_walk_and_train_when_faster() {
        let g = town();
        // To the far station the road is 700 blocks; a fast train beats it even with the transfer paid for.
        let speeds = Speeds { train: 400.0, ..Speeds::default() };
        let r = g.route(&[p(0.0, 0.0), p(300.0, -400.0)], &ALL, &speeds).unwrap();
        let modes: Vec<_> = r.legs.iter().map(|l| l.mode).collect();
        assert_eq!(modes, vec![Mode::Walk, Mode::Train]);
        assert!(r.seconds > speeds.transfer_penalty);
        let walk_only = g.route(&[p(0.0, 0.0), p(300.0, -400.0)], &[Mode::Walk], &speeds).unwrap();
        assert!(r.seconds < walk_only.seconds);
    }

    #[test]
    fn walking_wins_when_the_train_is_no_faster() {
        let g = town();
        let speeds = Speeds { train: 4.3, ..Speeds::default() };
        let r = g.route(&[p(0.0, 0.0), p(300.0, -100.0)], &ALL, &speeds).unwrap();
        assert!(r.legs.iter().all(|l| l.mode == Mode::Walk));
    }

    #[test]
    fn modes_not_allowed_are_not_used() {
        let g = town();
        let r = g.route(&[p(0.0, 0.0), p(300.0, -400.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        assert!(r.legs.iter().all(|l| l.mode == Mode::Walk));
    }

    #[test]
    fn a_transfer_needs_a_station() {
        let mut g = Graph::new();
        let a = g.add_node(0.0, 0.0, false);
        let b = g.add_node(100.0, 0.0, false); // not a station
        let c = g.add_node(200.0, 0.0, false);
        g.add_line(a, b, Mode::Walk, false).unwrap();
        g.add_line(b, c, Mode::Train, false).unwrap();
        let r = g.route(&[p(0.0, 0.0), p(200.0, 0.0)], &ALL, &Speeds::default());
        assert_eq!(r, Err(RouteError::Unreachable(1)));
    }

    #[test]
    fn one_way_lines_are_respected() {
        let mut g = Graph::new();
        let a = g.add_node(0.0, 0.0, false);
        let b = g.add_node(100.0, 0.0, false);
        g.add_line(a, b, Mode::Walk, true).unwrap();
        assert!(g.route(&[p(0.0, 0.0), p(100.0, 0.0)], &[Mode::Walk], &Speeds::default()).is_ok());
        // Against the arrow the network cannot help: the start has no way out, so the answer is a straight line.
        let back = g.route(&[p(100.0, 0.0), p(0.0, 0.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        assert!(back.legs.iter().all(|l| l.kind == LegKind::Direct));
    }

    #[test]
    fn a_closed_section_is_avoided() {
        let mut g = town();
        let open = g.route(&[p(0.0, 0.0), p(300.0, 0.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        assert!((open.length - 300.0).abs() < 1e-9);
        // Close b-c. Walking round by the far road is impossible without the rail, so with rail allowed it is used.
        g.set_closed(EdgeId(2), true);
        g.set_closed(EdgeId(3), true);
        let walk = g.route(&[p(0.0, 0.0), p(300.0, 0.0)], &[Mode::Walk], &Speeds::default());
        assert_eq!(walk, Err(RouteError::Unreachable(1)));
        let around = g.route(&[p(0.0, 0.0), p(300.0, 0.0)], &ALL, &Speeds::default()).unwrap();
        assert!(around.legs.iter().any(|l| l.mode == Mode::Train));
    }

    #[test]
    fn several_stops_are_visited_in_order() {
        let g = town();
        let stops = [p(0.0, 0.0), p(300.0, 0.0), p(100.0, 0.0)];
        let r = g.route(&stops, &[Mode::Walk], &Speeds::default()).unwrap();
        assert!((r.length - 500.0).abs() < 1e-9);
    }

    #[test]
    fn off_network_ends_are_straight_lines() {
        let g = town();
        let r = g.route(&[p(0.0, 30.0), p(300.0, 0.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        assert_eq!(r.legs[0].kind, LegKind::Direct);
        assert!((r.legs[0].length - 30.0).abs() < 1e-9);
        let far = g.route(&[p(5000.0, 5000.0), p(6000.0, 5000.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        assert_eq!((far.legs.len(), far.legs[0].kind), (1, LegKind::Direct));
        // Without walking there is no honest straight line.
        let train = g.route(&[p(5000.0, 5000.0), p(6000.0, 5000.0)], &[Mode::Train], &Speeds::default());
        assert_eq!(train, Err(RouteError::Unreachable(1)));
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        let g = town();
        let ok = Speeds::default();
        assert_eq!(g.route(&[p(0.0, 0.0)], &ALL, &ok), Err(RouteError::NoStops));
        assert_eq!(g.route(&vec![p(0.0, 0.0); MAX_STOPS + 1], &ALL, &ok), Err(RouteError::TooManyStops));
        for bad in [f64::NAN, 0.0, -1.0, f64::INFINITY] {
            let s = Speeds { walk: bad, ..ok };
            assert_eq!(g.route(&[p(0.0, 0.0), p(1.0, 1.0)], &ALL, &s), Err(RouteError::BadSpeeds));
        }
        let mut g2 = Graph::new();
        let a = g2.add_node(0.0, 0.0, false);
        assert_eq!(g2.add_line(a, a, Mode::Walk, false), Err(GraphError::SelfLoop));
        assert_eq!(g2.add_line(a, NodeId(9), Mode::Walk, false), Err(GraphError::UnknownNode));
    }

    #[test]
    fn speed_estimate_follows_the_player_within_limits() {
        let mut t = SpeedTracker::default();
        assert_eq!(t.speeds(Speeds::default()).walk, 4.3);
        for _ in 0..30 {
            t.record(Mode::Walk, 56.0, 10.0);
        }
        assert!((t.speeds(Speeds::default()).walk - 5.6).abs() < 0.01);
        t.record(Mode::Walk, 10.0, 0.0);
        t.record(Mode::Walk, f64::NAN, 1.0);
        t.record(Mode::Walk, -5.0, 1.0);
        assert!((t.speeds(Speeds::default()).walk - 5.6).abs() < 0.01);
        t.record(Mode::Boat, 1000.0, 1.0); // an absurd sample is clamped at 2x
        assert_eq!(t.speeds(Speeds::default()).boat, 16.0);
    }

    #[test]
    fn eta_uses_the_players_own_speed() {
        let g = town();
        let mut t = SpeedTracker::default();
        t.record(Mode::Walk, 86.0, 10.0);
        let stops = [p(0.0, 0.0), p(300.0, 0.0)];
        let slow = g.route(&stops, &[Mode::Walk], &Speeds::default()).unwrap();
        let fast = g.route(&stops, &[Mode::Walk], &t.speeds(Speeds::default())).unwrap();
        assert!(fast.seconds < slow.seconds);
    }

    #[test]
    fn headings_follow_the_compass() {
        let o = p(0.0, 0.0);
        assert_eq!(heading(o, p(0.0, -1.0)), 0.0);
        assert_eq!(heading(o, p(1.0, 0.0)), 90.0);
        assert_eq!(heading(o, p(0.0, 1.0)), 180.0);
        assert_eq!(heading(o, p(-1.0, 0.0)), 270.0);
    }

    #[test]
    fn hints_name_the_turns() {
        // North, then east (a right turn), then north again (a left turn).
        let points = vec![p(0.0, 0.0), p(0.0, -100.0), p(100.0, -100.0), p(100.0, -200.0), p(100.0, -300.0)];
        let leg = Leg { mode: Mode::Walk, kind: LegKind::Network, points, length: 400.0, seconds: 0.0 };
        let route = Route { legs: vec![leg], length: 400.0, seconds: 0.0 };
        let hints = route.hints();
        assert_eq!(hints.iter().map(|h| h.turn).collect::<Vec<_>>(), vec![Turn::Right, Turn::Left]);
        assert_eq!(hints[0].after, 100.0);
        assert_eq!(hints[1].after, 100.0);
    }

    #[test]
    fn turn_classes_cover_the_circle() {
        assert_eq!(classify(0.0), Turn::Straight);
        assert_eq!(classify(30.0), Turn::SlightRight);
        assert_eq!(classify(-90.0), Turn::Left);
        assert_eq!(classify(140.0), Turn::SharpRight);
        assert_eq!(classify(-170.0), Turn::UTurn);
    }

    #[test]
    fn locate_gives_distance_off_and_progress() {
        let g = town();
        let r = g.route(&[p(0.0, 0.0), p(300.0, 0.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        let (off, along) = r.locate(p(150.0, 10.0));
        assert!((off - 10.0).abs() < 1e-9 && (along - 150.0).abs() < 1e-9);
    }

    #[test]
    fn deviation_needs_three_samples_in_a_row() {
        let g = town();
        let r = g.route(&[p(0.0, 0.0), p(300.0, 0.0)], &[Mode::Walk], &Speeds::default()).unwrap();
        let mut d = Deviation::default();
        assert!(!d.sample(&r, p(150.0, 100.0)));
        assert!(!d.sample(&r, p(150.0, 100.0)));
        assert!(!d.sample(&r, p(150.0, 2.0))); // back on the road resets
        assert!(!d.sample(&r, p(150.0, 100.0)));
        assert!(!d.sample(&r, p(150.0, 100.0)));
        assert!(d.sample(&r, p(150.0, 100.0)));
        d.reset();
        assert!(!d.sample(&r, p(150.0, 100.0)));
    }

    #[test]
    fn a_large_grid_routes_quickly() {
        let n: u32 = 120;
        let mut g = Graph::new();
        let mut ids = Vec::new();
        for z in 0..n {
            for x in 0..n {
                ids.push(g.add_node(f64::from(x) * 10.0, f64::from(z) * 10.0, false));
            }
        }
        for z in 0..n {
            for x in 0..n {
                let i = (z * n + x) as usize;
                if x + 1 < n {
                    g.add_line(ids[i], ids[i + 1], Mode::Walk, false).unwrap();
                }
                if z + 1 < n {
                    g.add_line(ids[i], ids[i + n as usize], Mode::Walk, false).unwrap();
                }
            }
        }
        let end = f64::from(n - 1) * 10.0;
        let started = std::time::Instant::now();
        let r = g.route(&[p(0.0, 0.0), p(end, end)], &[Mode::Walk], &Speeds::default()).unwrap();
        assert!((r.length - 2.0 * end).abs() < 1e-6);
        assert!(started.elapsed().as_secs() < 5);
    }
}
