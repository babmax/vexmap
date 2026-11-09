// The route network editor's logic (Phase 6): what a click means for a tool, which node or line it hits, and which edit
// to send. Pure `std`, like ui.rs, so every rule is tested without the game. Also built as the `vexmap-editor` crate.
//
// The editor never decides whether a player may edit: it sends the edit and the sidecar answers from the database
// (rule 8). It only avoids sending what is plainly pointless (a second line over the same pair, a click on nothing).

use std::collections::HashMap;

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

/// What the client holds of the network. Ids are the server's.
#[derive(Clone, Debug, Default)]
pub struct Network {
    nodes: Vec<Node>,
    lines: Vec<Line>,
    by_id: HashMap<u64, usize>,
}

impl Network {
    pub fn new(nodes: Vec<Node>, lines: Vec<Line>) -> Self {
        let by_id = nodes.iter().enumerate().map(|(index, node)| (node.id, index)).collect();
        Network { nodes, lines, by_id }
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

    fn has_line(&self, a: u64, b: u64, mode: Mode) -> bool {
        self.lines.iter().any(|l| l.mode == mode && ((l.a == a && l.b == b) || (l.a == b && l.b == a)))
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
    AddNode { x: i32, z: i32, transfer: bool },
    AddLine { a: u64, b: u64, mode: Mode, one_way: bool },
    SetClosed { line: u64, closed: bool },
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
        net.nodes()
            .iter()
            .map(|n| (n, (f64::from(n.x) - world.0).hypot(f64::from(n.z) - world.1)))
            .filter(|(_, d)| *d <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(n, _)| *n)
    }

    fn line_at(net: &Network, world: (f64, f64), reach: f64) -> Option<Line> {
        net.lines()
            .iter()
            .filter_map(|l| {
                let (a, b) = (net.node(l.a)?, net.node(l.b)?);
                let d = distance_to_segment(world, (f64::from(a.x), f64::from(a.z)), (f64::from(b.x), f64::from(b.z)));
                (d <= reach).then_some((l, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(l, _)| *l)
    }

    /// What is under the cursor. Nodes win over lines, so a junction can be picked where lines meet.
    pub fn hover(&self, net: &Network, world: (f64, f64), px_per_block: f64) -> Hover {
        let reach = snap_blocks(px_per_block);
        if let Some(node) = Self::node_at(net, world, reach) {
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
    }

    #[test]
    fn changing_tool_drops_a_half_drawn_line() {
        let mut editor = Editor::default();
        editor.click(&net(), (0.0, 0.0), 1.0);
        editor.set_tool(Tool::Close);
        assert_eq!(editor.pending(), None);
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
