// The route network editor's logic (Phase 6): what a click means for a tool, which node or line it hits, and which edit
// to send. Pure `std`, like ui.rs, so every rule is tested without the game. Also built as the `vexmap-editor` crate.
//
// The editor never decides whether a player may edit: it sends the edit and the sidecar answers from the database
// (rule 8). It only avoids sending what is plainly pointless (a second line over the same pair, a click on nothing).

use std::collections::{HashMap, HashSet};

/// Same bound as the data core.
pub const MAX_COORDINATE: i64 = 30_000_000;
/// How close, on screen, a click must be to count as hitting a node or a line.
pub const SNAP_PIXELS: f64 = 10.0;
/// The hit radius in blocks stays between these, however far the map is zoomed.
const MIN_SNAP_BLOCKS: f64 = 1.0;
const MAX_SNAP_BLOCKS: f64 = 64.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Walk,
    Boat,
    Train,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: u64,
    pub x: i32,
    pub z: i32,
    pub transfer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub id: u64,
    pub a: u64,
    pub b: u64,
    pub mode: Mode,
    pub one_way: bool,
    pub closed: bool,
}

/// The side of a cell at the finest level, in blocks.
const CELL: f64 = 256.0;
/// `256 << 18` blocks is wider than any line the data core accepts.
const LEVELS: usize = 19;

/// A loose grid: an entry lives at the level whose cell is as wide as the entry, in the cell of its middle. A query only
/// has to look half a cell past its edges, so one long road never fills thousands of cells.
#[derive(Clone, Debug)]
struct Grid {
    levels: Vec<HashMap<(i32, i32), Vec<u32>>>,
}

impl Default for Grid {
    fn default() -> Self {
        Grid { levels: vec![HashMap::new(); LEVELS] }
    }
}

fn cell_size(level: usize) -> f64 {
    CELL * (1_u32 << level) as f64
}

fn level_for(extent: f64) -> usize {
    (0..LEVELS - 1).find(|level| cell_size(*level) >= extent).unwrap_or(LEVELS - 1)
}

impl Grid {
    fn insert(&mut self, middle: (f64, f64), extent: f64, index: u32) {
        let level = level_for(extent);
        let size = cell_size(level);
        let key = ((middle.0 / size).floor() as i32, (middle.1 / size).floor() as i32);
        self.levels[level].entry(key).or_default().push(index);
    }

    /// Calls `visit` for each entry that may touch the box, the widest entries first, until it returns `false`.
    fn query(&self, min: (f64, f64), max: (f64, f64), mut visit: impl FnMut(u32) -> bool) {
        for level in (0..LEVELS).rev() {
            let cells = &self.levels[level];
            if cells.is_empty() {
                continue;
            }
            let size = cell_size(level);
            let reach = size / 2.0;
            let first = (((min.0 - reach) / size).floor() as i64, ((min.1 - reach) / size).floor() as i64);
            let last = (((max.0 + reach) / size).floor() as i64, ((max.1 + reach) / size).floor() as i64);
            let wanted = (last.0 - first.0 + 1).saturating_mul(last.1 - first.1 + 1);
            if wanted > cells.len() as i64 {
                // A wide view over a sparse level: walking what exists is cheaper than probing every cell.
                for (key, entries) in cells {
                    let (x, z) = (i64::from(key.0), i64::from(key.1));
                    if x >= first.0 && x <= last.0 && z >= first.1 && z <= last.1 {
                        for entry in entries {
                            if !visit(*entry) {
                                return;
                            }
                        }
                    }
                }
                continue;
            }
            for x in first.0..=last.0 {
                for z in first.1..=last.1 {
                    let Some(entries) = cells.get(&(x as i32, z as i32)) else { continue };
                    for entry in entries {
                        if !visit(*entry) {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// What the client holds of the network. Ids are the server's. Rows are added and changed; a rollback removes them.
#[derive(Clone, Debug, Default)]
pub struct Network {
    nodes: Vec<Node>,
    lines: Vec<Line>,
    by_id: HashMap<u64, usize>,
    line_by_id: HashMap<u64, usize>,
    /// Lower end, higher end, mode: "is there already a line here" without walking them all.
    pairs: HashSet<(u64, u64, Mode)>,
    node_grid: Grid,
    line_grid: Grid,
}

fn pair(a: u64, b: u64, mode: Mode) -> (u64, u64, Mode) {
    (a.min(b), a.max(b), mode)
}

impl Network {
    pub fn new(nodes: Vec<Node>, lines: Vec<Line>) -> Self {
        let mut network = Network::default();
        network.apply(&nodes, &lines);
        network
    }

    /// Adds these rows, or replaces the ones already known. Nodes go first, so a line can find its ends.
    pub fn apply(&mut self, nodes: &[Node], lines: &[Line]) {
        for node in nodes {
            self.upsert_node(*node);
        }
        for line in lines {
            self.upsert_line(*line);
        }
    }

    /// Drops rows that were rolled back. Rare, so the grids are simply built again. Returns whether anything went.
    pub fn remove(&mut self, nodes: &[u64], lines: &[u64]) -> bool {
        let gone_nodes: HashSet<u64> = nodes.iter().copied().filter(|id| self.by_id.contains_key(id)).collect();
        let gone_lines: HashSet<u64> = lines.iter().copied().filter(|id| self.line_by_id.contains_key(id)).collect();
        if gone_nodes.is_empty() && gone_lines.is_empty() {
            return false;
        }
        let kept_nodes = self.nodes.iter().filter(|node| !gone_nodes.contains(&node.id)).copied().collect();
        let kept_lines = self.lines.iter().filter(|line| !gone_lines.contains(&line.id)).copied().collect();
        *self = Network::new(kept_nodes, kept_lines);
        true
    }

    fn upsert_node(&mut self, node: Node) {
        if let Some(index) = self.by_id.get(&node.id) {
            let old = &mut self.nodes[*index];
            // A node never moves. If a server ever said otherwise, the grid would need to follow.
            *old = Node { x: old.x, z: old.z, ..node };
            return;
        }
        let index = self.nodes.len();
        self.by_id.insert(node.id, index);
        self.node_grid.insert((f64::from(node.x), f64::from(node.z)), 0.0, index as u32);
        self.nodes.push(node);
    }

    fn upsert_line(&mut self, line: Line) {
        if let Some(index) = self.line_by_id.get(&line.id) {
            let old = &mut self.lines[*index];
            // The ends and the mode are fixed when a line is drawn; only the closed state changes.
            *old = Line { a: old.a, b: old.b, mode: old.mode, ..line };
            return;
        }
        // A line whose ends are unknown cannot be placed. Pages arrive in order, so it does not happen.
        let (Some(a), Some(b)) = (self.node(line.a).copied(), self.node(line.b).copied()) else { return };
        let index = self.lines.len();
        self.line_by_id.insert(line.id, index);
        self.pairs.insert(pair(line.a, line.b, line.mode));
        let (ax, az, bx, bz) = (f64::from(a.x), f64::from(a.z), f64::from(b.x), f64::from(b.z));
        let extent = (ax - bx).abs().max((az - bz).abs());
        self.line_grid.insert(((ax + bx) / 2.0, (az + bz) / 2.0), extent, index as u32);
        self.lines.push(line);
    }

    /// The lines in a box, the longest first, at most `limit`. A view too crowded to draw loses its shortest roads.
    pub fn lines_in(&self, min: (f64, f64), max: (f64, f64), limit: usize) -> Vec<Line> {
        let mut found = Vec::new();
        self.line_grid.query(min, max, |index| {
            let line = self.lines[index as usize];
            let (Some(a), Some(b)) = (self.node(line.a), self.node(line.b)) else { return true };
            let (low, high) = ((a.x.min(b.x), a.z.min(b.z)), (a.x.max(b.x), a.z.max(b.z)));
            let touches = f64::from(high.0) >= min.0
                && f64::from(low.0) <= max.0
                && f64::from(high.1) >= min.1
                && f64::from(low.1) <= max.1;
            if touches {
                found.push(line);
            }
            found.len() < limit
        });
        found
    }

    /// The nodes in a box, at most `limit`.
    pub fn nodes_in(&self, min: (f64, f64), max: (f64, f64), limit: usize) -> Vec<Node> {
        let mut found = Vec::new();
        self.node_grid.query(min, max, |index| {
            let node = self.nodes[index as usize];
            let (x, z) = (f64::from(node.x), f64::from(node.z));
            if x >= min.0 && x <= max.0 && z >= min.1 && z <= max.1 {
                found.push(node);
            }
            found.len() < limit
        });
        found
    }

    pub fn len_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn node(&self, id: u64) -> Option<&Node> {
        self.by_id.get(&id).map(|index| &self.nodes[*index])
    }

    // Read by tests.
    #[allow(dead_code)]
    pub fn line(&self, id: u64) -> Option<&Line> {
        self.line_by_id.get(&id).map(|index| &self.lines[*index])
    }

    fn has_line(&self, a: u64, b: u64, mode: Mode) -> bool {
        self.pairs.contains(&pair(a, b, mode))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// Click to place a node.
    Node,
    /// Click a node, then another (or empty ground, which places a node and goes on from it).
    Line,
    /// Click a line to close it, or to reopen it.
    Close,
}

/// An edit to send. The same shapes as the wire's `NetworkEdit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    AddNode {
        x: i32,
        z: i32,
        transfer: bool,
    },
    AddLine {
        a: u64,
        b: u64,
        mode: Mode,
        one_way: bool,
    },
    SetClosed {
        line: u64,
        closed: bool,
    },
    /// Undoes the edit with this journal number.
    Revert {
        seq: u64,
    },
}

/// A message for the player, by key (the strings live in the language files).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    OutsideWorld,
    NodeExists,
    LineExists,
    NothingThere,
    Forbidden,
    Invalid,
    Unavailable,
    /// The network has no dimension of its own yet: it is drawn on the overworld.
    WrongDimension,
    PickDestination,
    NoRoute,
    Arrived,
    PickStop,
    TooManyStops,
    StopReached,
    /// The sidecar would not undo it: a node that still has lines, or something else stands there now.
    RevertRefused,
}

impl Notice {
    /// English and Russian, as the map's other texts.
    pub fn text(self) -> (&'static str, &'static str) {
        match self {
            Notice::OutsideWorld => ("That is outside the world", "Это за пределами мира"),
            Notice::NodeExists => ("There is a node here already", "Здесь уже есть узел"),
            Notice::LineExists => ("That line already exists", "Такая линия уже есть"),
            Notice::NothingThere => ("Click a node or a line", "Нажмите на узел или линию"),
            Notice::Forbidden => ("You may not edit the network", "У вас нет прав на правку сети"),
            Notice::Invalid => ("The server refused this edit", "Сервер отклонил правку"),
            Notice::Unavailable => ("The map server is not answering", "Сервер карты не отвечает"),
            Notice::WrongDimension => ("The network is on the overworld", "Сеть рисуется только в обычном мире"),
            Notice::PickDestination => ("Click where to go", "Нажмите на точку назначения"),
            Notice::NoRoute => ("No route to there", "Туда нет маршрута"),
            Notice::Arrived => ("You have arrived", "Вы прибыли"),
            Notice::PickStop => {
                ("Click to add a stop, click a stop to remove it", "Нажмите: новая остановка, на метку: убрать")
            }
            Notice::TooManyStops => ("A route has 15 stops at most", "В маршруте не больше 15 остановок"),
            Notice::StopReached => ("Stop reached", "Остановка пройдена"),
            Notice::RevertRefused => {
                ("Cannot be undone yet: undo its lines first", "Пока нельзя: сначала откатите его линии")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    /// Send this edit; its result comes back under `token`.
    Send {
        token: u32,
        edit: Edit,
    },
    /// The first end of a line was chosen.
    Picked(u64),
    /// Drawing was cancelled by clicking the same node again.
    Cancelled,
    Notice(Notice),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    Nothing,
    Node(u64),
    Line(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditStatus {
    Done,
    AlreadyDone,
    Forbidden,
    Invalid,
    Unavailable,
}

pub struct Editor {
    pub tool: Tool,
    pub mode: Mode,
    pub one_way: bool,
    /// New nodes are stations (the mode may change there).
    pub transfer: bool,
    /// The line being drawn starts at this node.
    pending: Option<u64>,
    /// An edit that placed a node to continue a line from `from`: when it succeeds, the line follows.
    chain: Option<(u32, u64)>,
    next_token: u32,
}

impl Default for Editor {
    fn default() -> Self {
        Editor {
            tool: Tool::Line,
            mode: Mode::Walk,
            one_way: false,
            transfer: false,
            pending: None,
            chain: None,
            next_token: 1,
        }
    }
}

fn snap_blocks(px_per_block: f64) -> f64 {
    if px_per_block.is_finite() && px_per_block > 0.0 {
        (SNAP_PIXELS / px_per_block).clamp(MIN_SNAP_BLOCKS, MAX_SNAP_BLOCKS)
    } else {
        MAX_SNAP_BLOCKS
    }
}

fn distance_to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dz) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dz * dz;
    let t = if len2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dz) / len2).clamp(0.0, 1.0) };
    (p.0 - (a.0 + t * dx)).hypot(p.1 - (a.1 + t * dz))
}

impl Editor {
    pub fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.cancel();
    }

    /// Escape, or changing tool.
    pub fn cancel(&mut self) {
        self.pending = None;
        self.chain = None;
    }

    /// The node a line is being drawn from, for the rubber-band preview.
    pub fn pending(&self) -> Option<u64> {
        self.pending
    }

    fn token(&mut self) -> u32 {
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1).max(1);
        token
    }

    fn node_at(net: &Network, world: (f64, f64), reach: f64) -> Option<Node> {
        let mut best: Option<(Node, f64)> = None;
        net.node_grid.query((world.0 - reach, world.1 - reach), (world.0 + reach, world.1 + reach), |index| {
            let node = net.nodes[index as usize];
            let distance = (f64::from(node.x) - world.0).hypot(f64::from(node.z) - world.1);
            if distance <= reach && best.is_none_or(|(_, nearest)| distance < nearest) {
                best = Some((node, distance));
            }
            true
        });
        best.map(|(node, _)| node)
    }

    fn line_at(net: &Network, world: (f64, f64), reach: f64) -> Option<Line> {
        let mut best: Option<(Line, f64)> = None;
        net.line_grid.query((world.0 - reach, world.1 - reach), (world.0 + reach, world.1 + reach), |index| {
            let line = net.lines[index as usize];
            let (Some(a), Some(b)) = (net.node(line.a), net.node(line.b)) else { return true };
            let ends = ((f64::from(a.x), f64::from(a.z)), (f64::from(b.x), f64::from(b.z)));
            let distance = distance_to_segment(world, ends.0, ends.1);
            if distance <= reach && best.is_none_or(|(_, nearest)| distance < nearest) {
                best = Some((line, distance));
            }
            true
        });
        best.map(|(line, _)| line)
    }

    /// What a click here would act on. Nodes win over lines, so a junction can be picked where lines meet, except for the
    /// closing tool, which only works on lines.
    pub fn hover(&self, net: &Network, world: (f64, f64), px_per_block: f64) -> Hover {
        let reach = snap_blocks(px_per_block);
        if self.tool != Tool::Close
            && let Some(node) = Self::node_at(net, world, reach)
        {
            return Hover::Node(node.id);
        }
        match Self::line_at(net, world, reach) {
            Some(line) if self.tool != Tool::Node => Hover::Line(line.id),
            _ => Hover::Nothing,
        }
    }

    fn place(&mut self, world: (f64, f64)) -> Result<(i32, i32), Notice> {
        let (x, z) = (world.0.round(), world.1.round());
        if !(x.is_finite() && z.is_finite()) || x.abs() > MAX_COORDINATE as f64 || z.abs() > MAX_COORDINATE as f64 {
            return Err(Notice::OutsideWorld);
        }
        Ok((x as i32, z as i32))
    }

    /// A left click at a world point, with the map's current scale.
    pub fn click(&mut self, net: &Network, world: (f64, f64), px_per_block: f64) -> Click {
        let reach = snap_blocks(px_per_block);
        let node = Self::node_at(net, world, reach);
        match self.tool {
            Tool::Node => {
                if node.is_some() {
                    return Click::Notice(Notice::NodeExists);
                }
                match self.place(world) {
                    Ok((x, z)) => {
                        let token = self.token();
                        Click::Send { token, edit: Edit::AddNode { x, z, transfer: self.transfer } }
                    }
                    Err(notice) => Click::Notice(notice),
                }
            }
            Tool::Line => self.click_line(net, world, node),
            Tool::Close => match Self::line_at(net, world, reach) {
                Some(line) => {
                    let token = self.token();
                    Click::Send { token, edit: Edit::SetClosed { line: line.id, closed: !line.closed } }
                }
                None => Click::Notice(Notice::NothingThere),
            },
        }
    }

    fn click_line(&mut self, net: &Network, world: (f64, f64), node: Option<Node>) -> Click {
        let Some(from) = self.pending else {
            return match node {
                Some(node) => {
                    self.pending = Some(node.id);
                    Click::Picked(node.id)
                }
                None => Click::Notice(Notice::NothingThere),
            };
        };
        match node {
            Some(node) if node.id == from => {
                self.cancel();
                Click::Cancelled
            }
            Some(node) => {
                if net.has_line(from, node.id, self.mode) {
                    return Click::Notice(Notice::LineExists);
                }
                let token = self.token();
                // Keep drawing from the node just reached: a road is a chain of clicks.
                self.pending = Some(node.id);
                Click::Send {
                    token,
                    edit: Edit::AddLine { a: from, b: node.id, mode: self.mode, one_way: self.one_way },
                }
            }
            None => match self.place(world) {
                Ok((x, z)) => {
                    let token = self.token();
                    self.chain = Some((token, from));
                    Click::Send { token, edit: Edit::AddNode { x, z, transfer: self.transfer } }
                }
                Err(notice) => Click::Notice(notice),
            },
        }
    }

    /// The server's answer to an edit. Returns the follow-up edit of a chain, or a notice for the player.
    pub fn on_result(&mut self, token: u32, status: EditStatus, id: u64) -> Option<Result<Click, Notice>> {
        let chained = self.chain.filter(|(t, _)| *t == token);
        if chained.is_some() {
            self.chain = None;
        }
        match status {
            EditStatus::Forbidden => {
                self.cancel();
                Some(Err(Notice::Forbidden))
            }
            EditStatus::Invalid => {
                self.cancel();
                Some(Err(Notice::Invalid))
            }
            EditStatus::Unavailable => {
                self.cancel();
                Some(Err(Notice::Unavailable))
            }
            EditStatus::Done | EditStatus::AlreadyDone => {
                let (_, from) = chained?;
                if id == 0 {
                    return None;
                }
                // The new node exists: draw the line to it and carry on from it.
                self.pending = Some(id);
                let token = self.token();
                Some(Ok(Click::Send {
                    token,
                    edit: Edit::AddLine { a: from, b: id, mode: self.mode, one_way: self.one_way },
                }))
            }
        }
    }
}

// ---------------------------------------------------------------- following the server

/// How long an answer may take before the question is asked again: plugin channels may lose a message.
const ANSWER_TIMEOUT_MS: u64 = 3_000;
/// With the map up, other people's edits show this soon.
const POLL_WITH_MAP_MS: u64 = 2_000;
const POLL_IDLE_MS: u64 = 30_000;
/// How long a message stays on the panel.
const NOTICE_MS: u64 = 4_000;
/// Where a route can go, the start not counted: `vexmap_routing::MAX_STOPS` less one.
pub const MAX_ROUTE_STOPS: usize = 15;
/// Tokens of rollbacks waiting for an answer. Only so that a refusal can be worded properly.
const MAX_REVERTS_IN_THE_AIR: usize = 32;

/// One page from the server, in the editor's types.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub since: u64,
    pub to: u64,
    pub latest: u64,
    pub may_edit: bool,
    pub render_done: u32,
    pub render_total: u32,
    pub rendering: bool,
    pub nodes: Vec<Node>,
    pub lines: Vec<Line>,
    pub removed_nodes: Vec<u64>,
    pub removed_lines: Vec<u64>,
}

/// What an edit of the network did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryKind {
    NodeAdded,
    LineAdded,
    LineClosed,
    LineOpened,
    NodeRemoved,
    LineRemoved,
}

/// One line of the network's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub seq: u64,
    pub age_seconds: u32,
    /// Empty for a mod or a player never seen.
    pub actor: String,
    pub kind: HistoryKind,
    pub place: Option<(i32, i32)>,
    /// It no longer stands, so there is nothing to undo.
    pub undone: bool,
}

impl HistoryRow {
    pub fn describe(&self, ru: bool) -> String {
        let age = match (self.age_seconds, ru) {
            (0..60, false) => "now".to_owned(),
            (0..60, true) => "сейчас".to_owned(),
            (seconds @ 60..3600, false) => format!("{} min", seconds / 60),
            (seconds @ 60..3600, true) => format!("{} мин", seconds / 60),
            (seconds @ 3600..86_400, false) => format!("{} h", seconds / 3600),
            (seconds @ 3600..86_400, true) => format!("{} ч", seconds / 3600),
            (seconds, false) => format!("{} d", seconds / 86_400),
            (seconds, true) => format!("{} д", seconds / 86_400),
        };
        let what = match (self.kind, ru) {
            (HistoryKind::NodeAdded, false) => "node added",
            (HistoryKind::NodeAdded, true) => "добавлен узел",
            (HistoryKind::LineAdded, false) => "line added",
            (HistoryKind::LineAdded, true) => "добавлена линия",
            (HistoryKind::LineClosed, false) => "line closed",
            (HistoryKind::LineClosed, true) => "закрыта линия",
            (HistoryKind::LineOpened, false) => "line opened",
            (HistoryKind::LineOpened, true) => "открыта линия",
            (HistoryKind::NodeRemoved, false) => "node removed",
            (HistoryKind::NodeRemoved, true) => "убран узел",
            (HistoryKind::LineRemoved, false) => "line removed",
            (HistoryKind::LineRemoved, true) => "убрана линия",
        };
        let who = match (self.actor.is_empty(), ru) {
            (true, false) => "a mod",
            (true, true) => "мод",
            (false, _) => &self.actor,
        };
        format!("{age}  {who}: {what}")
    }
}

/// One button of the editor's panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Opens the editor, or closes it.
    Toggle,
    Tool(Tool),
    Mode(Mode),
    OneWay,
    Transfer,
    /// Picks a destination, or drops the route being followed.
    Route,
    /// Builds a new route after the player left the old one.
    Reroute,
    /// Draws the whole map now.
    Render,
    /// The next clicks on the map add stops to the route being followed.
    AddStop,
    /// Drops the last stop of the route.
    RemoveStop,
    /// Shows or hides who changed what in the network, and how to undo it.
    History,
}

impl Button {
    pub fn label(self, editing: bool) -> (&'static str, &'static str) {
        match self {
            Button::Toggle if editing => ("Done", "Готово"),
            Button::Toggle => ("Edit roads", "Правка дорог"),
            Button::Tool(Tool::Node) => ("Node", "Узел"),
            Button::Tool(Tool::Line) => ("Line", "Линия"),
            Button::Tool(Tool::Close) => ("Close", "Закрыть"),
            Button::Mode(Mode::Walk) => ("Walk", "Пешком"),
            Button::Mode(Mode::Boat) => ("Boat", "Лодка"),
            Button::Mode(Mode::Train) => ("Train", "Поезд"),
            Button::OneWay => ("One way", "В 1 сторону"),
            Button::Transfer => ("Station", "Станция"),
            Button::Route => ("Route", "Маршрут"),
            Button::Reroute => ("Reroute", "Перестроить"),
            Button::Render => ("Load map", "Прогрузить"),
            Button::AddStop => ("+ Stop", "+ Остановка"),
            Button::RemoveStop => ("- Stop", "- Остановка"),
            Button::History => ("History", "Журнал"),
        }
    }
}

const EDITOR_BUTTONS: [Button; 10] = [
    Button::Toggle,
    Button::Tool(Tool::Node),
    Button::Tool(Tool::Line),
    Button::Tool(Tool::Close),
    Button::Mode(Mode::Walk),
    Button::Mode(Mode::Boat),
    Button::Mode(Mode::Train),
    Button::OneWay,
    Button::Transfer,
    Button::History,
];

/// Everything the client keeps about the network: the copy of it, how far that copy is behind, and the open editor.
#[derive(Default)]
pub struct Workspace {
    pub network: Network,
    pub editor: Editor,
    pub editing: bool,
    pub may_edit: bool,
    /// The player's client and this region both took `routing`.
    enabled: bool,
    have: u64,
    latest: u64,
    /// When the question now in the air was asked.
    asked_at: Option<u64>,
    answered_at: u64,
    /// Do not wait for the next poll: there is more to fetch, or our own edit should show.
    ask_now: bool,
    notice: Option<(Notice, u64)>,
    /// Edits waiting for the next frame, when the game can be written to. Input hooks can only read it.
    outbox: Vec<(u32, Edit)>,
    /// The next click on the map is where to go.
    pub picking_route: bool,
    /// And the ones after it too: the player is adding stops, until told otherwise.
    keep_picking: bool,
    /// Where the route goes or is about to go, the start not counted. Told by whoever holds the route after each search,
    /// and changed here at once, so a second quick click builds on the first.
    pub route_stops: Vec<(f64, f64)>,
    /// A route is being followed, and whether the player has left it. Told each frame by whoever holds the route.
    pub route_shown: bool,
    pub off_route: bool,
    route_request: Option<Vec<(f64, f64)>>,
    route_dropped: bool,
    route_again: bool,
    want_render: bool,
    /// Regions of the world drawn, all of them, and whether a full drawing is running.
    pub render: (u32, u32, bool),
    /// The history panel is up, and the page in it.
    pub history_open: bool,
    pub history: Vec<HistoryRow>,
    history_more: bool,
    /// The `before` of each page gone through. The last is the one shown; the first is always 0, the newest.
    history_pages: Vec<u64>,
    history_asked_at: Option<u64>,
    history_ask_now: bool,
    reverts: Vec<u32>,
}

impl Workspace {
    /// A server welcomed us. It may be another network entirely, so what we hold is dropped and fetched again.
    pub fn welcomed(&mut self, enabled: bool) {
        *self = Workspace { enabled, ask_now: true, editor: std::mem::take(&mut self.editor), ..Workspace::default() };
        self.editor.cancel();
    }

    /// The `since` to ask about now, if a question is due. `map_up` asks more often.
    pub fn want_due(&mut self, now_ms: u64, map_up: bool) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        if let Some(at) = self.asked_at {
            if now_ms.saturating_sub(at) < ANSWER_TIMEOUT_MS {
                return None;
            }
        } else if !self.ask_now {
            let wait = if map_up { POLL_WITH_MAP_MS } else { POLL_IDLE_MS };
            if now_ms.saturating_sub(self.answered_at) < wait {
                return None;
            }
        }
        self.asked_at = Some(now_ms);
        self.ask_now = false;
        Some(self.have)
    }

    /// Applies an answer. Returns whether the network changed.
    pub fn page(&mut self, page: &Page, now_ms: u64) -> bool {
        if page.since != self.have {
            return false; // A late copy of an old answer.
        }
        self.asked_at = None;
        self.answered_at = now_ms;
        self.may_edit = page.may_edit;
        self.render = (page.render_done, page.render_total, page.rendering);
        if !page.may_edit {
            self.editing = false;
            self.history_open = false;
        }
        if page.latest < page.since {
            // The server's database is older than what we hold: it was restored or replaced.
            let (editor, enabled) = (std::mem::take(&mut self.editor), self.enabled);
            *self = Workspace { editor, enabled, may_edit: page.may_edit, ask_now: true, ..Workspace::default() };
            self.editor.cancel();
            return true;
        }
        self.have = page.to.max(page.since);
        self.latest = page.latest;
        self.network.apply(&page.nodes, &page.lines);
        let removed = self.network.remove(&page.removed_nodes, &page.removed_lines);
        if self.editor.pending().is_some_and(|node| self.network.node(node).is_none()) {
            self.editor.cancel();
        }
        self.ask_now = self.have < page.latest;
        let changed = removed || !page.nodes.is_empty() || !page.lines.is_empty();
        if changed && self.history_open {
            // Someone else's edit, or ours: the list is out of date.
            self.history_ask_now = true;
        }
        changed
    }

    /// Still downloading the network.
    pub fn syncing(&self) -> bool {
        self.have < self.latest
    }

    /// The next network request also asks for the whole map. Taken once.
    pub fn take_want_render(&mut self) -> bool {
        std::mem::take(&mut self.want_render)
    }

    /// Where the player chose to go, once.
    pub fn take_route_request(&mut self) -> Option<Vec<(f64, f64)>> {
        self.route_request.take()
    }

    pub fn take_route_dropped(&mut self) -> bool {
        std::mem::take(&mut self.route_dropped)
    }

    pub fn take_route_again(&mut self) -> bool {
        std::mem::take(&mut self.route_again)
    }

    /// A route result the player should hear about.
    pub fn tell(&mut self, notice: Notice, now_ms: u64) {
        self.say(notice, now_ms);
    }

    /// The panel's buttons, each with whether it is lit. Empty until this region has the routing module.
    pub fn buttons(&self) -> Vec<(Button, bool)> {
        if !self.enabled {
            return Vec::new();
        }
        if !self.editing {
            let mut buttons = vec![(Button::Route, self.picking_route || self.route_shown)];
            if self.route_shown {
                buttons.push((Button::AddStop, self.picking_route));
                if self.route_stops.len() > 1 {
                    buttons.push((Button::RemoveStop, false));
                }
                if self.off_route {
                    buttons.push((Button::Reroute, false));
                }
            }
            if self.may_edit {
                buttons.push((Button::Toggle, false));
                buttons.push((Button::Render, self.render.2));
            }
            return buttons;
        }
        EDITOR_BUTTONS
            .iter()
            .map(|button| {
                let lit = match button {
                    Button::Toggle
                    | Button::Route
                    | Button::Reroute
                    | Button::Render
                    | Button::AddStop
                    | Button::RemoveStop => false,
                    Button::History => self.history_open,
                    Button::Tool(tool) => self.editor.tool == *tool,
                    Button::Mode(mode) => self.editor.mode == *mode,
                    Button::OneWay => self.editor.one_way,
                    Button::Transfer => self.editor.transfer,
                };
                (*button, lit)
            })
            .collect()
    }

    pub fn press(&mut self, button: Button) {
        match button {
            Button::Toggle => {
                self.editing = !self.editing;
                self.editor.cancel();
                self.history_open = false;
            }
            Button::Tool(tool) => self.editor.set_tool(tool),
            Button::Mode(mode) => self.editor.mode = mode,
            Button::OneWay => self.editor.one_way = !self.editor.one_way,
            Button::Transfer => self.editor.transfer = !self.editor.transfer,
            Button::Route if self.route_shown => {
                self.route_dropped = true;
                self.picking_route = false;
                self.keep_picking = false;
                self.route_stops.clear();
            }
            Button::Route => {
                self.picking_route = !self.picking_route;
                self.keep_picking = false;
            }
            Button::AddStop => {
                self.picking_route = !self.picking_route;
                self.keep_picking = self.picking_route;
            }
            Button::RemoveStop => {
                if self.route_stops.len() > 1 {
                    self.route_stops.pop();
                    self.route_request = Some(self.route_stops.clone());
                }
            }
            Button::Reroute => self.route_again = true,
            Button::History => {
                self.history_open = !self.history_open;
                self.history.clear();
                self.history_more = false;
                self.history_pages = vec![0];
                self.history_asked_at = None;
                self.history_ask_now = self.history_open;
            }
            Button::Render => {
                self.want_render = true;
                self.ask_now = true;
            }
        }
    }

    fn say(&mut self, notice: Notice, now_ms: u64) {
        self.notice = Some((notice, now_ms + NOTICE_MS));
    }

    pub fn notice(&self, now_ms: u64) -> Option<Notice> {
        self.notice.filter(|(_, until)| now_ms < *until).map(|(notice, _)| notice)
    }

    /// A left click on the map while editing. `on_overworld`: the network is not drawn anywhere else.
    pub fn click(&mut self, world: (f64, f64), px_per_block: f64, on_overworld: bool, now_ms: u64) {
        if !on_overworld {
            self.say(Notice::WrongDimension, now_ms);
            return;
        }
        if self.picking_route {
            self.picking_route = self.keep_picking;
            self.pick_stop(world, px_per_block, now_ms);
            return;
        }
        match self.editor.click(&self.network, world, px_per_block) {
            Click::Send { token, edit } => self.outbox.push((token, edit)),
            Click::Notice(notice) => self.say(notice, now_ms),
            Click::Picked(_) | Click::Cancelled => {}
        }
    }

    /// A click while choosing where to go: on a stop it removes that stop, anywhere else it adds one.
    fn pick_stop(&mut self, world: (f64, f64), px_per_block: f64, now_ms: u64) {
        let reach = snap_blocks(px_per_block);
        let hit = self.route_stops.iter().position(|stop| (stop.0 - world.0).hypot(stop.1 - world.1) <= reach);
        if let Some(index) = hit {
            self.route_stops.remove(index);
            if self.route_stops.is_empty() {
                self.route_dropped = true;
                self.picking_route = false;
                self.keep_picking = false;
            } else {
                self.route_request = Some(self.route_stops.clone());
            }
            return;
        }
        if self.route_stops.len() >= MAX_ROUTE_STOPS {
            self.say(Notice::TooManyStops, now_ms);
            return;
        }
        self.route_stops.push(world);
        self.route_request = Some(self.route_stops.clone());
    }

    /// The player is adding stops, rather than choosing the one place to go.
    pub fn adding_stops(&self) -> bool {
        self.picking_route && self.keep_picking
    }

    /// The `before` to ask the server about, if a page of the history is due.
    pub fn history_due(&mut self, now_ms: u64) -> Option<u64> {
        if !self.history_open {
            return None;
        }
        if let Some(at) = self.history_asked_at {
            if now_ms.saturating_sub(at) < ANSWER_TIMEOUT_MS {
                return None;
            }
        } else if !self.history_ask_now {
            return None;
        }
        self.history_asked_at = Some(now_ms);
        self.history_ask_now = false;
        self.history_pages.last().copied()
    }

    /// A page of the history from the server. A late copy of another page is dropped.
    pub fn history_page(&mut self, before: u64, more: bool, rows: Vec<HistoryRow>) {
        if !self.history_open || self.history_pages.last() != Some(&before) {
            return;
        }
        self.history = rows;
        self.history_more = more;
        self.history_asked_at = None;
    }

    pub fn history_has_older(&self) -> bool {
        self.history_more && !self.history.is_empty()
    }

    pub fn history_has_newer(&self) -> bool {
        self.history_pages.len() > 1
    }

    pub fn history_older(&mut self) {
        if let (true, Some(oldest)) = (self.history_has_older(), self.history.last()) {
            self.history_pages.push(oldest.seq);
            self.history_asked_at = None;
            self.history_ask_now = true;
        }
    }

    pub fn history_newer(&mut self) {
        if self.history_has_newer() {
            self.history_pages.pop();
            self.history_asked_at = None;
            self.history_ask_now = true;
        }
    }

    /// Asks the server to undo the edit on this line of the panel. Sent on the next frame.
    pub fn revert(&mut self, row: usize) {
        let Some(seq) = self.history.get(row).filter(|row| !row.undone).map(|row| row.seq) else { return };
        let token = self.editor.token();
        self.outbox.push((token, Edit::Revert { seq }));
        if self.reverts.len() >= MAX_REVERTS_IN_THE_AIR {
            self.reverts.remove(0);
        }
        self.reverts.push(token);
    }

    /// Where the edit on this line of the panel happened.
    pub fn history_place(&self, row: usize) -> Option<(i32, i32)> {
        self.history.get(row)?.place
    }

    /// The right button: only drops a half-drawn line, so a stray click never leaves the editor.
    pub fn cancel_line(&mut self) -> bool {
        let drawing = self.editor.pending().is_some();
        self.editor.cancel();
        drawing
    }

    /// A click outside the world's circle.
    pub fn outside_world(&mut self, now_ms: u64) {
        self.say(Notice::OutsideWorld, now_ms);
    }

    /// Escape. `false` when there was nothing to step back from, so the key may close the map.
    pub fn step_back(&mut self) -> bool {
        if std::mem::take(&mut self.picking_route) {
            self.keep_picking = false;
            return true;
        }
        if self.editor.pending().is_some() {
            self.editor.cancel();
            return true;
        }
        if std::mem::take(&mut self.history_open) {
            return true;
        }
        std::mem::take(&mut self.editing)
    }

    /// The server's answer to an edit.
    pub fn result(&mut self, token: u32, status: EditStatus, id: u64, now_ms: u64) {
        if let Some(index) = self.reverts.iter().position(|sent| *sent == token) {
            self.reverts.remove(index);
            match status {
                EditStatus::Done | EditStatus::AlreadyDone => {
                    self.ask_now = true;
                    self.history_ask_now = true;
                }
                EditStatus::Forbidden => self.say(Notice::Forbidden, now_ms),
                EditStatus::Invalid => {
                    self.say(Notice::RevertRefused, now_ms);
                    self.history_ask_now = true;
                }
                EditStatus::Unavailable => self.say(Notice::Unavailable, now_ms),
            }
            return;
        }
        match self.editor.on_result(token, status, id) {
            Some(Ok(Click::Send { token, edit })) => self.outbox.push((token, edit)),
            Some(Err(notice)) => self.say(notice, now_ms),
            _ => {}
        }
        if matches!(status, EditStatus::Done | EditStatus::AlreadyDone) {
            // Our own edit should show at once, not at the next poll.
            self.ask_now = true;
        }
    }

    /// Edits to send now.
    pub fn take_outbox(&mut self) -> Vec<(u32, Edit)> {
        std::mem::take(&mut self.outbox)
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    fn page(since: u64, to: u64, latest: u64, nodes: Vec<Node>, lines: Vec<Line>) -> Page {
        Page { since, to, latest, may_edit: true, nodes, lines, ..Page::default() }
    }

    fn node(id: u64, x: i32, z: i32) -> Node {
        Node { id, x, z, transfer: false }
    }

    fn line(id: u64, a: u64, b: u64) -> Line {
        Line { id, a, b, mode: Mode::Walk, one_way: false, closed: false }
    }

    fn welcomed() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.welcomed(true);
        workspace
    }

    #[test]
    fn nothing_is_asked_without_routing() {
        let mut workspace = Workspace::default();
        workspace.welcomed(false);
        assert_eq!(workspace.want_due(0, true), None);
        assert!(workspace.buttons().is_empty());
    }

    #[test]
    fn pages_are_followed_until_the_copy_is_current() {
        let mut workspace = welcomed();
        assert_eq!(workspace.want_due(0, false), Some(0));
        assert_eq!(workspace.want_due(10, false), None, "one question at a time");
        assert!(workspace.page(&page(0, 2, 3, vec![node(1, 0, 0), node(2, 9, 0)], vec![]), 20));
        assert!(workspace.syncing());
        assert_eq!(workspace.want_due(20, false), Some(2), "the next page follows at once");
        assert!(workspace.page(&page(2, 3, 3, vec![], vec![line(3, 1, 2)]), 40));
        assert!(!workspace.syncing());
        assert_eq!(workspace.network.lines().len(), 1);
        assert_eq!(workspace.want_due(50, false), None, "up to date: nothing until the next poll");
    }

    #[test]
    fn a_lost_answer_is_asked_for_again() {
        let mut workspace = welcomed();
        assert_eq!(workspace.want_due(0, false), Some(0));
        assert_eq!(workspace.want_due(2_999, false), None);
        assert_eq!(workspace.want_due(3_000, false), Some(0));
    }

    #[test]
    fn a_late_copy_of_an_old_answer_changes_nothing() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 5, 5, vec![node(1, 0, 0)], vec![]), 1);
        assert!(!workspace.page(&page(0, 9, 9, vec![node(7, 5, 5)], vec![]), 2));
        assert_eq!(workspace.network.nodes().len(), 1);
    }

    #[test]
    fn the_map_being_up_makes_polls_more_frequent() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 5, 5, vec![], vec![]), 1000);
        assert_eq!(workspace.want_due(2_000, true), None);
        assert_eq!(workspace.want_due(3_000, true), Some(5));
        workspace.page(&page(5, 5, 5, vec![], vec![]), 3000);
        assert_eq!(workspace.want_due(10_000, false), None, "with the map down it waits the long way");
        assert_eq!(workspace.want_due(33_000, false), Some(5));
    }

    #[test]
    fn a_restored_database_starts_the_copy_again() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 50, 50, vec![node(1, 0, 0)], vec![]), 1);
        workspace.ask_now = true;
        assert_eq!(workspace.want_due(5, false), Some(50));
        assert!(workspace.page(&page(50, 10, 10, vec![], vec![]), 6));
        assert!(workspace.network.nodes().is_empty());
        assert_eq!(workspace.want_due(7, false), Some(0));
    }

    #[test]
    fn the_editor_is_only_offered_to_who_may_edit_and_is_taken_away_with_the_right() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        let refused = Page { may_edit: false, ..Page::default() };
        workspace.page(&refused, 1);
        assert_eq!(workspace.buttons(), vec![(Button::Route, false)], "everyone may ask for a route");
        workspace.want_due(40_000, false);
        workspace.page(&page(0, 0, 0, vec![], vec![]), 40_001);
        assert_eq!(workspace.buttons(), vec![(Button::Route, false), (Button::Toggle, false), (Button::Render, false)]);
        workspace.press(Button::Toggle);
        assert_eq!(workspace.buttons().len(), EDITOR_BUTTONS.len());
        workspace.want_due(80_000, false);
        workspace.page(&refused, 80_001);
        assert!(!workspace.editing);
        assert_eq!(workspace.buttons(), vec![(Button::Route, false)]);
    }

    #[test]
    fn panel_buttons_change_the_editor_and_light_up() {
        let mut workspace = welcomed();
        workspace.may_edit = true;
        workspace.press(Button::Toggle);
        workspace.press(Button::Mode(Mode::Train));
        workspace.press(Button::Tool(Tool::Close));
        workspace.press(Button::OneWay);
        let lit: Vec<Button> = workspace.buttons().into_iter().filter(|(_, lit)| *lit).map(|(b, _)| b).collect();
        assert_eq!(lit, vec![Button::Tool(Tool::Close), Button::Mode(Mode::Train), Button::OneWay]);
    }

    #[test]
    fn a_click_queues_an_edit_and_an_answer_brings_the_next_page_at_once() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 1, 1, vec![node(1, 0, 0)], vec![]), 1);
        workspace.editor.set_tool(Tool::Node);
        workspace.click((50.0, 50.0), 1.0, true, 2);
        let sent = workspace.take_outbox();
        assert_eq!(sent.len(), 1);
        assert!(workspace.take_outbox().is_empty());
        assert_eq!(workspace.want_due(3, true), None);
        workspace.result(sent[0].0, EditStatus::Done, 2, 4);
        assert_eq!(workspace.want_due(5, true), Some(1));
    }

    #[test]
    fn a_chain_of_clicks_sends_the_line_after_the_node() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 1, 1, vec![node(1, 0, 0)], vec![]), 1);
        workspace.click((0.0, 0.0), 1.0, true, 2);
        workspace.click((200.0, 0.0), 1.0, true, 3);
        let (token, edit) = workspace.take_outbox()[0];
        assert_eq!(edit, Edit::AddNode { x: 200, z: 0, transfer: false });
        workspace.result(token, EditStatus::Done, 9, 4);
        let (_, edit) = workspace.take_outbox()[0];
        assert_eq!(edit, Edit::AddLine { a: 1, b: 9, mode: Mode::Walk, one_way: false });
    }

    #[test]
    fn notices_come_from_refusals_and_fade() {
        let mut workspace = welcomed();
        workspace.click((0.0, 0.0), 1.0, false, 100);
        assert_eq!(workspace.notice(101), Some(Notice::WrongDimension));
        assert!(workspace.take_outbox().is_empty(), "nothing is drawn from another dimension");
        assert_eq!(workspace.notice(100 + NOTICE_MS), None);
        workspace.result(1, EditStatus::Forbidden, 0, 200);
        assert_eq!(workspace.notice(201), Some(Notice::Forbidden));
    }

    #[test]
    fn stepping_back_first_drops_the_line_then_leaves_the_editor_then_lets_the_key_through() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 1, 1, vec![node(1, 0, 0)], vec![]), 1);
        workspace.press(Button::Toggle);
        workspace.click((0.0, 0.0), 1.0, true, 2);
        assert_eq!(workspace.editor.pending(), Some(1));
        assert!(workspace.step_back());
        assert_eq!(workspace.editor.pending(), None);
        assert!(workspace.editing);
        assert!(workspace.step_back());
        assert!(!workspace.editing);
        assert!(!workspace.step_back());
    }

    #[test]
    fn the_route_button_picks_a_place_and_a_click_asks_for_it_once() {
        let mut workspace = welcomed();
        workspace.press(Button::Route);
        assert!(workspace.picking_route);
        assert_eq!(workspace.buttons()[0], (Button::Route, true));
        workspace.click((50.0, 60.0), 1.0, true, 1);
        assert!(!workspace.picking_route);
        assert_eq!(workspace.take_route_request(), Some(vec![(50.0, 60.0)]));
        assert_eq!(workspace.take_route_request(), None);
        assert!(workspace.take_outbox().is_empty(), "choosing a place is not an edit");
    }

    #[test]
    fn with_a_route_the_button_drops_it_and_leaving_it_offers_another() {
        let mut workspace = welcomed();
        workspace.route_shown = true;
        assert!(workspace.buttons().iter().all(|(button, _)| *button != Button::Reroute));
        workspace.off_route = true;
        assert!(workspace.buttons().contains(&(Button::Reroute, false)));
        workspace.press(Button::Reroute);
        assert!(workspace.take_route_again());
        workspace.press(Button::Route);
        assert!(workspace.take_route_dropped());
        assert!(!workspace.picking_route);
    }

    fn following(stops: &[(f64, f64)]) -> Workspace {
        let mut workspace = welcomed();
        workspace.route_shown = true;
        workspace.route_stops = stops.to_vec();
        workspace
    }

    #[test]
    fn stops_are_added_one_click_after_another_until_the_button_is_pressed_again() {
        let mut workspace = following(&[(100.0, 0.0)]);
        assert!(workspace.buttons().contains(&(Button::AddStop, false)));
        workspace.press(Button::AddStop);
        assert!(workspace.adding_stops());
        assert!(workspace.buttons().contains(&(Button::AddStop, true)));
        workspace.click((200.0, 0.0), 1.0, true, 1);
        assert_eq!(workspace.take_route_request(), Some(vec![(100.0, 0.0), (200.0, 0.0)]));
        workspace.click((300.0, 0.0), 1.0, true, 2);
        assert_eq!(workspace.take_route_request(), Some(vec![(100.0, 0.0), (200.0, 0.0), (300.0, 0.0)]));
        assert!(workspace.picking_route, "still adding");
        workspace.press(Button::AddStop);
        assert!(!workspace.picking_route);
        assert!(workspace.take_outbox().is_empty(), "stops are not edits");
    }

    #[test]
    fn a_click_on_a_stop_removes_it() {
        let mut workspace = following(&[(100.0, 0.0), (200.0, 0.0), (300.0, 0.0)]);
        workspace.press(Button::AddStop);
        workspace.click((201.0, 2.0), 1.0, true, 1);
        assert_eq!(workspace.take_route_request(), Some(vec![(100.0, 0.0), (300.0, 0.0)]));
    }

    #[test]
    fn removing_the_only_stop_drops_the_route() {
        let mut workspace = following(&[(100.0, 0.0)]);
        workspace.press(Button::AddStop);
        workspace.click((100.0, 0.0), 1.0, true, 1);
        assert!(workspace.take_route_dropped());
        assert_eq!(workspace.take_route_request(), None);
        assert!(!workspace.picking_route);
    }

    #[test]
    fn the_last_stop_can_be_dropped_but_never_the_only_one() {
        let mut workspace = following(&[(100.0, 0.0)]);
        assert!(workspace.buttons().iter().all(|(button, _)| *button != Button::RemoveStop));
        workspace.press(Button::RemoveStop);
        assert_eq!(workspace.take_route_request(), None);
        let mut workspace = following(&[(100.0, 0.0), (200.0, 0.0)]);
        workspace.press(Button::RemoveStop);
        assert_eq!(workspace.take_route_request(), Some(vec![(100.0, 0.0)]));
    }

    #[test]
    fn a_route_has_a_limit_of_stops() {
        let stops: Vec<(f64, f64)> = (0..MAX_ROUTE_STOPS).map(|i| (i as f64 * 1000.0, 0.0)).collect();
        let mut workspace = following(&stops);
        workspace.press(Button::AddStop);
        workspace.click((0.0, 9000.0), 1.0, true, 1);
        assert_eq!(workspace.take_route_request(), None);
        assert_eq!(workspace.notice(2), Some(Notice::TooManyStops));
    }

    #[test]
    fn dropping_the_route_forgets_the_stops() {
        let mut workspace = following(&[(100.0, 0.0), (200.0, 0.0)]);
        workspace.press(Button::Route);
        assert!(workspace.take_route_dropped());
        assert!(workspace.route_stops.is_empty());
    }

    fn row(seq: u64, undone: bool) -> HistoryRow {
        HistoryRow {
            seq,
            age_seconds: 90,
            actor: "Steve".into(),
            kind: HistoryKind::LineAdded,
            place: Some((4, 5)),
            undone,
        }
    }

    fn with_history() -> Workspace {
        let mut workspace = welcomed();
        workspace.may_edit = true;
        workspace.press(Button::Toggle);
        workspace.press(Button::History);
        workspace
    }

    #[test]
    fn the_history_is_asked_for_once_and_again_after_a_lost_answer() {
        assert_eq!(welcomed().history_due(0), None, "closed");
        let mut workspace = with_history();
        assert_eq!(workspace.history_due(0), Some(0));
        assert_eq!(workspace.history_due(100), None);
        assert_eq!(workspace.history_due(3_000), Some(0));
    }

    #[test]
    fn the_history_is_paged_back_and_forth() {
        let mut workspace = with_history();
        workspace.history_due(0);
        workspace.history_page(0, true, vec![row(9, false), row(7, false)]);
        assert!(workspace.history_has_older() && !workspace.history_has_newer());
        workspace.history_older();
        assert_eq!(workspace.history_due(1), Some(7));
        workspace.history_page(0, false, vec![]);
        assert_eq!(workspace.history.len(), 2, "a late copy of the page before");
        workspace.history_page(7, false, vec![row(3, false)]);
        assert_eq!(workspace.history[0].seq, 3);
        assert!(!workspace.history_has_older() && workspace.history_has_newer());
        workspace.history_newer();
        assert_eq!(workspace.history_due(2), Some(0));
    }

    #[test]
    fn a_rollback_is_sent_and_its_answer_refreshes_the_list_and_the_network() {
        let mut workspace = with_history();
        workspace.history_due(0);
        workspace.history_page(0, false, vec![row(9, false), row(7, true)]);
        workspace.revert(1);
        assert!(workspace.take_outbox().is_empty(), "what no longer stands cannot be undone");
        workspace.revert(0);
        let sent = workspace.take_outbox();
        assert_eq!(sent[0].1, Edit::Revert { seq: 9 });
        workspace.result(sent[0].0, EditStatus::Done, 12, 1);
        assert_eq!(workspace.history_due(2), Some(0));
        assert_eq!(workspace.want_due(3, true), Some(0));
    }

    #[test]
    fn a_refused_rollback_says_why_and_keeps_a_half_drawn_line() {
        let mut workspace = with_history();
        workspace.want_due(0, false);
        workspace.page(&page(0, 1, 1, vec![node(1, 0, 0)], vec![]), 1);
        workspace.click((0.0, 0.0), 1.0, true, 2);
        workspace.history_due(3);
        workspace.history_page(0, false, vec![row(1, false)]);
        workspace.revert(0);
        let sent = workspace.take_outbox();
        workspace.result(sent[0].0, EditStatus::Invalid, 0, 4);
        assert_eq!(workspace.notice(5), Some(Notice::RevertRefused));
        assert_eq!(workspace.editor.pending(), Some(1));
    }

    #[test]
    fn someone_elses_edit_refreshes_an_open_history() {
        let mut workspace = with_history();
        workspace.history_due(0);
        workspace.history_page(0, false, vec![]);
        assert_eq!(workspace.history_due(1), None);
        workspace.want_due(0, false);
        workspace.page(&page(0, 1, 1, vec![node(1, 0, 0)], vec![]), 2);
        assert_eq!(workspace.history_due(3), Some(0));
    }

    #[test]
    fn a_rolled_back_row_leaves_the_copy_and_a_line_drawn_from_it_is_dropped() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 3, 3, vec![node(1, 0, 0), node(2, 9, 0)], vec![line(3, 1, 2)]), 1);
        workspace.click((9.0, 0.0), 1.0, true, 2);
        assert_eq!(workspace.editor.pending(), Some(2));
        workspace.want_due(10_000, false);
        let removal = Page { removed_lines: vec![3], removed_nodes: vec![2], ..page(3, 5, 5, vec![], vec![]) };
        assert!(workspace.page(&removal, 10_001));
        assert_eq!(workspace.network.lines().len(), 0);
        assert_eq!(workspace.network.nodes().len(), 1);
        assert_eq!(workspace.editor.pending(), None);
    }

    #[test]
    fn escape_closes_the_history_before_the_editor() {
        let mut workspace = with_history();
        assert!(workspace.step_back());
        assert!(!workspace.history_open && workspace.editing);
        assert!(workspace.step_back());
        assert!(!workspace.editing);
    }

    #[test]
    fn leaving_the_editor_closes_the_history() {
        let mut workspace = with_history();
        workspace.press(Button::Toggle);
        assert!(!workspace.history_open);
        assert_eq!(workspace.history_due(0), None);
    }

    #[test]
    fn a_history_row_reads_like_a_sentence() {
        assert_eq!(row(1, false).describe(true), "1 мин  Steve: добавлена линия");
        assert_eq!(row(1, false).describe(false), "1 min  Steve: line added");
        let old =
            HistoryRow { age_seconds: 200_000, actor: String::new(), kind: HistoryKind::NodeRemoved, ..row(1, false) };
        assert_eq!(old.describe(true), "2 д  мод: убран узел");
        assert_eq!(HistoryRow { age_seconds: 5, ..row(1, false) }.describe(false), "now  Steve: line added");
        assert_eq!(HistoryRow { age_seconds: 7200, ..row(1, false) }.describe(true), "2 ч  Steve: добавлена линия");
    }

    #[test]
    fn escape_cancels_choosing_a_place_first() {
        let mut workspace = welcomed();
        workspace.press(Button::Route);
        assert!(workspace.step_back());
        assert!(!workspace.picking_route);
        assert!(!workspace.step_back());
    }

    #[test]
    fn the_load_button_asks_at_once_and_only_once_and_shows_it_is_running() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 0, 0, vec![], vec![]), 1);
        workspace.press(Button::Render);
        assert_eq!(workspace.want_due(2, false), Some(0), "without waiting for the next poll");
        assert!(workspace.take_want_render());
        assert!(!workspace.take_want_render());
        let running = Page { may_edit: true, render_done: 5, render_total: 9, rendering: true, ..Page::default() };
        workspace.page(&running, 3);
        assert_eq!(workspace.render, (5, 9, true));
        assert!(workspace.buttons().contains(&(Button::Render, true)));
    }

    #[test]
    fn a_new_server_drops_the_copy_and_the_half_drawn_line() {
        let mut workspace = welcomed();
        workspace.want_due(0, false);
        workspace.page(&page(0, 1, 1, vec![node(1, 0, 0)], vec![]), 1);
        workspace.click((0.0, 0.0), 1.0, true, 2);
        workspace.welcomed(true);
        assert!(workspace.network.nodes().is_empty());
        assert_eq!(workspace.editor.pending(), None);
        assert_eq!(workspace.want_due(3, false), Some(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, x: i32, z: i32) -> Node {
        Node { id, x, z, transfer: false }
    }

    fn line(id: u64, a: u64, b: u64) -> Line {
        Line { id, a, b, mode: Mode::Walk, one_way: false, closed: false }
    }

    fn net() -> Network {
        Network::new(vec![node(1, 0, 0), node(2, 100, 0), node(3, 100, 100)], vec![line(10, 1, 2)])
    }

    fn sent(click: Click) -> (u32, Edit) {
        match click {
            Click::Send { token, edit } => (token, edit),
            other => panic!("expected a send, got {other:?}"),
        }
    }

    #[test]
    fn the_node_tool_places_a_node_on_the_nearest_block() {
        let mut editor = Editor { tool: Tool::Node, transfer: true, ..Editor::default() };
        let click = editor.click(&net(), (250.4, -30.6), 1.0);
        assert_eq!(sent(click).1, Edit::AddNode { x: 250, z: -31, transfer: true });
    }

    #[test]
    fn a_node_cannot_be_placed_on_a_node_or_outside_the_world() {
        let mut editor = Editor { tool: Tool::Node, ..Editor::default() };
        assert_eq!(editor.click(&net(), (3.0, 2.0), 1.0), Click::Notice(Notice::NodeExists));
        assert_eq!(editor.click(&net(), (31_000_000.0, 0.0), 1.0), Click::Notice(Notice::OutsideWorld));
        assert_eq!(editor.click(&net(), (f64::NAN, 0.0), 1.0), Click::Notice(Notice::OutsideWorld));
    }

    #[test]
    fn two_clicks_on_nodes_draw_a_line_and_drawing_goes_on() {
        let mut editor = Editor::default();
        assert_eq!(editor.click(&net(), (100.0, 0.0), 1.0), Click::Picked(2));
        let (_, edit) = sent(editor.click(&net(), (100.0, 100.0), 1.0));
        assert_eq!(edit, Edit::AddLine { a: 2, b: 3, mode: Mode::Walk, one_way: false });
        assert_eq!(editor.pending(), Some(3), "the road continues from the node just reached");
    }

    #[test]
    fn clicking_the_same_node_cancels() {
        let mut editor = Editor::default();
        editor.click(&net(), (0.0, 0.0), 1.0);
        assert_eq!(editor.click(&net(), (1.0, 1.0), 1.0), Click::Cancelled);
        assert_eq!(editor.pending(), None);
    }

    #[test]
    fn a_second_line_over_the_same_pair_is_not_sent() {
        let mut editor = Editor::default();
        editor.click(&net(), (100.0, 0.0), 1.0);
        assert_eq!(editor.click(&net(), (0.0, 0.0), 1.0), Click::Notice(Notice::LineExists));
        // Another mode over the same pair is a different line.
        editor.mode = Mode::Train;
        assert!(matches!(editor.click(&net(), (0.0, 0.0), 1.0), Click::Send { .. }));
    }

    #[test]
    fn a_line_must_start_on_a_node() {
        let mut editor = Editor::default();
        assert_eq!(editor.click(&net(), (500.0, 500.0), 1.0), Click::Notice(Notice::NothingThere));
    }

    #[test]
    fn clicking_empty_ground_places_a_node_and_then_the_line() {
        let mut editor = Editor { one_way: true, mode: Mode::Boat, ..Editor::default() };
        editor.click(&net(), (100.0, 100.0), 1.0);
        let (token, edit) = sent(editor.click(&net(), (300.0, 100.0), 1.0));
        assert_eq!(edit, Edit::AddNode { x: 300, z: 100, transfer: false });
        let follow = editor.on_result(token, EditStatus::Done, 77).unwrap().unwrap();
        let (_, edit) = sent(follow);
        assert_eq!(edit, Edit::AddLine { a: 3, b: 77, mode: Mode::Boat, one_way: true });
        assert_eq!(editor.pending(), Some(77));
        // A result for something else does not trigger a line.
        assert_eq!(editor.on_result(token, EditStatus::Done, 78), None);
    }

    #[test]
    fn a_refused_chain_stops_drawing_with_a_reason() {
        let mut editor = Editor::default();
        editor.click(&net(), (100.0, 100.0), 1.0);
        let (token, _) = sent(editor.click(&net(), (300.0, 100.0), 1.0));
        assert_eq!(editor.on_result(token, EditStatus::Forbidden, 0), Some(Err(Notice::Forbidden)));
        assert_eq!(editor.pending(), None);
        assert_eq!(editor.on_result(token, EditStatus::Done, 5), None, "the chain is gone");
    }

    #[test]
    fn the_close_tool_toggles_the_line_under_the_cursor() {
        let mut editor = Editor { tool: Tool::Close, ..Editor::default() };
        let (_, edit) = sent(editor.click(&net(), (50.0, 3.0), 1.0));
        assert_eq!(edit, Edit::SetClosed { line: 10, closed: true });
        let mut closed = line(10, 1, 2);
        closed.closed = true;
        let net = Network::new(vec![node(1, 0, 0), node(2, 100, 0)], vec![closed]);
        let (_, edit) = sent(editor.click(&net, (50.0, 0.0), 1.0));
        assert_eq!(edit, Edit::SetClosed { line: 10, closed: false });
        assert_eq!(editor.click(&net, (50.0, 80.0), 1.0), Click::Notice(Notice::NothingThere));
    }

    #[test]
    fn the_hit_radius_follows_the_zoom_within_limits() {
        let editor = Editor::default();
        // Zoomed in (10 px per block): 1 block is reachable, 5 are not.
        assert_eq!(editor.hover(&net(), (1.0, 0.0), 10.0), Hover::Node(1));
        assert_eq!(editor.hover(&net(), (5.0, 5.0), 10.0), Hover::Nothing);
        // Zoomed far out (0.01 px per block): capped at 64 blocks, not 1000.
        assert_eq!(editor.hover(&net(), (500.0, 500.0), 0.01), Hover::Nothing);
        assert_eq!(editor.hover(&net(), (60.0, 0.0), 0.01), Hover::Node(2));
    }

    #[test]
    fn nodes_beat_lines_and_the_node_tool_ignores_lines() {
        let mut editor = Editor::default();
        assert_eq!(editor.hover(&net(), (0.0, 2.0), 1.0), Hover::Node(1));
        assert_eq!(editor.hover(&net(), (50.0, 2.0), 1.0), Hover::Line(10));
        editor.tool = Tool::Node;
        assert_eq!(editor.hover(&net(), (50.0, 2.0), 1.0), Hover::Nothing);
        editor.tool = Tool::Close;
        assert_eq!(editor.hover(&net(), (1.0, 2.0), 1.0), Hover::Line(10), "the closing tool only acts on lines");
    }

    #[test]
    fn changing_tool_drops_a_half_drawn_line() {
        let mut editor = Editor::default();
        editor.click(&net(), (0.0, 0.0), 1.0);
        editor.set_tool(Tool::Close);
        assert_eq!(editor.pending(), None);
    }

    #[test]
    fn removing_rows_leaves_the_rest_findable_and_they_can_come_back() {
        let mut network = net();
        assert!(!network.remove(&[99], &[98]), "nothing we hold");
        assert!(network.remove(&[], &[10]));
        assert!(network.lines().is_empty());
        let editor = Editor::default();
        assert_eq!(editor.hover(&network, (50.0, 2.0), 1.0), Hover::Nothing);
        assert_eq!(editor.hover(&network, (100.0, 2.0), 1.0), Hover::Node(2));
        let mut drawing = Editor::default();
        drawing.click(&network, (0.0, 0.0), 1.0);
        assert!(matches!(drawing.click(&network, (100.0, 0.0), 1.0), Click::Send { .. }), "the pair is free again");
        network.apply(&[], &[line(10, 1, 2)]);
        assert_eq!(editor.hover(&network, (50.0, 2.0), 1.0), Hover::Line(10));
    }

    #[test]
    fn a_closed_line_replaces_the_open_one() {
        let mut network = net();
        let mut closed = line(10, 1, 2);
        closed.closed = true;
        network.apply(&[], &[closed]);
        assert_eq!(network.lines().len(), 1);
        assert!(network.line(10).unwrap().closed);
    }

    #[test]
    fn a_query_finds_long_lines_from_any_part_of_them_and_the_longest_come_first() {
        let nodes = vec![node(1, -20_000_000, 5), node(2, 20_000_000, 5), node(3, 0, 40), node(4, 30, 40)];
        let network = Network::new(nodes, vec![line(10, 3, 4), line(11, 1, 2)]);
        let found = network.lines_in((-5.0, -5.0), (35.0, 45.0), 10);
        assert_eq!(found.iter().map(|l| l.id).collect::<Vec<_>>(), vec![11, 10]);
        assert_eq!(network.lines_in((-5.0, -5.0), (35.0, 45.0), 1)[0].id, 11, "the short road goes first");
        assert!(network.lines_in((0.0, 900.0), (35.0, 945.0), 10).is_empty());
        let editor = Editor::default();
        assert_eq!(editor.hover(&network, (15_000_000.0, 6.0), 1.0), Hover::Line(11));
    }

    #[test]
    fn nodes_are_cut_to_the_box_and_the_limit() {
        let network = Network::new(vec![node(1, 0, 0), node(2, 10, 0), node(3, 20, 0), node(4, 9_000, 0)], vec![]);
        assert_eq!(network.nodes_in((-1.0, -1.0), (11.0, 1.0), 10).len(), 2);
        assert_eq!(network.nodes_in((-1.0, -1.0), (21.0, 1.0), 2).len(), 2);
        assert_eq!(network.nodes_in((-100_000.0, -100_000.0), (100_000.0, 100_000.0), 10).len(), 4);
    }

    #[test]
    fn a_huge_network_answers_clicks_without_walking_all_of_it() {
        let mut nodes = Vec::new();
        let mut lines = Vec::new();
        for x in 0..400 {
            for z in 0..400 {
                nodes.push(node(u64::from(x as u32 * 400 + z as u32 + 1), x * 50, z * 50));
            }
        }
        for index in 0..159_000_u64 {
            lines.push(line(1_000_000 + index, index + 1, index + 2));
        }
        let network = Network::new(nodes, lines);
        let editor = Editor::default();
        let started = std::time::Instant::now();
        for step in 0..2000 {
            assert_ne!(editor.hover(&network, (f64::from(step % 400) * 50.0, 100.0), 4.0), Hover::Nothing);
        }
        // Linear search would take seconds here even in a release build.
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    }

    #[test]
    fn tokens_are_never_zero_and_differ() {
        let mut editor = Editor { tool: Tool::Node, ..Editor::default() };
        editor.next_token = u32::MAX;
        let (a, _) = sent(editor.click(&net(), (500.0, 500.0), 1.0));
        let (b, _) = sent(editor.click(&net(), (600.0, 500.0), 1.0));
        assert!(a != 0 && b != 0 && a != b);
    }
}
