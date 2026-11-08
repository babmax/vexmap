// VexMap wire protocol: frames, splitting, reassembly, handshake and admission. Knows nothing of Horizon and uses only std,
// so the same file is the `vexmap-proto` crate (tests, the future sidecar) and the `vexmap_proto` module injected into the
// game. Module payloads cannot carry inner attributes or `mod` files: keep this one self-contained file.
//
// Everything a client sends is untrusted input. Nothing here panics on bad bytes, and every buffer is bounded.

use std::collections::HashMap;

pub const CHANNEL: &str = "vexmap:main";
pub const PROTOCOL: u16 = 1;
pub const FRAME_VERSION: u8 = 1;
pub const MAGIC: [u8; 2] = *b"VM";
pub const HEADER_LEN: usize = 12;
/// A client's plugin message is at most 32 767 bytes (spike S5). Room is left for the packet's own header.
pub const MAX_C2S_FRAME: usize = 32_000;
/// A server's plugin message is at most 1 MiB (spike S5).
pub const MAX_S2C_FRAME: usize = 1_000_000;
pub const MAX_PARTS: u16 = 4096;
pub const MAX_MOD_VERSION: usize = 64;
pub const MAX_REASON: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hello = 1,
    Welcome = 2,
    Reject = 3,
    Ping = 4,
    Pong = 5,
    Probe = 6,
}

impl Kind {
    pub fn from_u8(value: u8) -> Option<Kind> {
        Some(match value {
            1 => Kind::Hello,
            2 => Kind::Welcome,
            3 => Kind::Reject,
            4 => Kind::Ping,
            5 => Kind::Pong,
            6 => Kind::Probe,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub message: u32,
    pub part: u16,
    pub parts: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    TooShort,
    BadMagic,
    BadVersion(u8),
    BadParts,
    TooLarge,
}

/// Splits `body` into frames of at most `max_frame` bytes each, header included.
pub fn encode(kind: Kind, message: u32, body: &[u8], max_frame: usize) -> Result<Vec<Vec<u8>>, FrameError> {
    if max_frame <= HEADER_LEN {
        return Err(FrameError::TooLarge);
    }
    let room = max_frame - HEADER_LEN;
    let parts = body.len().div_ceil(room).max(1);
    if parts > usize::from(MAX_PARTS) {
        return Err(FrameError::TooLarge);
    }
    let mut frames = Vec::with_capacity(parts);
    for part in 0..parts {
        let chunk = &body[(part * room).min(body.len())..((part + 1) * room).min(body.len())];
        let mut frame = Vec::with_capacity(HEADER_LEN + chunk.len());
        frame.extend_from_slice(&MAGIC);
        frame.push(FRAME_VERSION);
        frame.push(kind as u8);
        frame.extend_from_slice(&message.to_le_bytes());
        frame.extend_from_slice(&(part as u16).to_le_bytes());
        frame.extend_from_slice(&(parts as u16).to_le_bytes());
        frame.extend_from_slice(chunk);
        frames.push(frame);
    }
    Ok(frames)
}

pub fn parse_frame(bytes: &[u8]) -> Result<(Header, &[u8]), FrameError> {
    if bytes.len() < HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes[..2] != MAGIC {
        return Err(FrameError::BadMagic);
    }
    if bytes[2] != FRAME_VERSION {
        return Err(FrameError::BadVersion(bytes[2]));
    }
    let header = Header {
        kind: bytes[3],
        message: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        part: u16::from_le_bytes([bytes[8], bytes[9]]),
        parts: u16::from_le_bytes([bytes[10], bytes[11]]),
    };
    if header.parts == 0 || header.parts > MAX_PARTS || header.part >= header.parts {
        return Err(FrameError::BadParts);
    }
    Ok((header, &bytes[HEADER_LEN..]))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello { protocol: u16, features: u64, mod_version: String },
    Welcome { protocol: u16, features: u64 },
    Reject { reason: String },
    Ping(u64),
    Pong(u64),
    Probe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    UnknownKind(u8),
    Truncated,
    TrailingBytes,
    BadString,
}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::Hello { .. } => Kind::Hello,
            Message::Welcome { .. } => Kind::Welcome,
            Message::Reject { .. } => Kind::Reject,
            Message::Ping(_) => Kind::Ping,
            Message::Pong(_) => Kind::Pong,
            Message::Probe => Kind::Probe,
        }
    }

    pub fn body(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Message::Hello { protocol, features, mod_version } => {
                out.extend_from_slice(&protocol.to_le_bytes());
                out.extend_from_slice(&features.to_le_bytes());
                put_str(&mut out, mod_version, MAX_MOD_VERSION);
            }
            Message::Welcome { protocol, features } => {
                out.extend_from_slice(&protocol.to_le_bytes());
                out.extend_from_slice(&features.to_le_bytes());
            }
            Message::Reject { reason } => put_str(&mut out, reason, MAX_REASON),
            Message::Ping(stamp) | Message::Pong(stamp) => out.extend_from_slice(&stamp.to_le_bytes()),
            Message::Probe => {}
        }
        out
    }

    pub fn decode(kind: u8, body: &[u8]) -> Result<Message, DecodeError> {
        let mut reader = Reader { bytes: body };
        let message = match Kind::from_u8(kind).ok_or(DecodeError::UnknownKind(kind))? {
            Kind::Hello => Message::Hello {
                protocol: reader.u16()?,
                features: reader.u64()?,
                mod_version: reader.string(MAX_MOD_VERSION)?,
            },
            Kind::Welcome => Message::Welcome { protocol: reader.u16()?, features: reader.u64()? },
            Kind::Reject => Message::Reject { reason: reader.string(MAX_REASON)? },
            Kind::Ping => Message::Ping(reader.u64()?),
            Kind::Pong => Message::Pong(reader.u64()?),
            Kind::Probe => Message::Probe,
        };
        if !reader.bytes.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(message)
    }
}

/// Writes at most `max` bytes of `text`, cut at a character boundary.
fn put_str(out: &mut Vec<u8>, text: &str, max: usize) {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    out.extend_from_slice(&(end as u16).to_le_bytes());
    out.extend_from_slice(&text.as_bytes()[..end]);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        if self.bytes.len() < count {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(head)
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        let mut b = [0; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }

    fn string(&mut self, max: usize) -> Result<String, DecodeError> {
        let len = usize::from(self.u16()?);
        if len > max {
            return Err(DecodeError::BadString);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| DecodeError::BadString)
    }
}

/// Numbers outgoing messages and splits them into frames.
#[derive(Default)]
pub struct Outbox {
    next: u32,
}

impl Outbox {
    pub fn frames(&mut self, message: &Message, max_frame: usize) -> Result<Vec<Vec<u8>>, FrameError> {
        let number = self.next;
        self.next = self.next.wrapping_add(1);
        encode(message.kind(), number, &message.body(), max_frame)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Messages being reassembled at once. The oldest is dropped to make room.
    pub max_pending: usize,
    /// Bytes held by unfinished messages.
    pub max_bytes: usize,
    /// An unfinished message older than this is dropped.
    pub timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_pending: 8, max_bytes: 4 << 20, timeout_ms: 30_000 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    Frame(FrameError),
    Duplicate,
    Mismatch,
    OverBudget,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Feed {
    Complete { kind: u8, body: Vec<u8> },
    Partial,
    Dropped(DropReason),
}

struct Pending {
    kind: u8,
    parts: Vec<Option<Vec<u8>>>,
    received: u16,
    bytes: usize,
    started_ms: u64,
}

/// Puts one sender's frames back into messages. One per peer: a client has one, a server one per player.
pub struct Reassembler {
    limits: Limits,
    pending: HashMap<u32, Pending>,
    bytes: usize,
}

impl Reassembler {
    pub fn new(limits: Limits) -> Self {
        Reassembler { limits, pending: HashMap::new(), bytes: 0 }
    }

    // Read by tests and the sidecar, not by the game.
    #[allow(dead_code)]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    // Read by tests and the sidecar, not by the game.
    #[allow(dead_code)]
    pub fn held_bytes(&self) -> usize {
        self.bytes
    }

    pub fn feed(&mut self, frame: &[u8], now_ms: u64) -> Feed {
        self.expire(now_ms);
        let (header, data) = match parse_frame(frame) {
            Ok(parsed) => parsed,
            Err(error) => return Feed::Dropped(DropReason::Frame(error)),
        };
        if header.parts == 1 {
            return Feed::Complete { kind: header.kind, body: data.to_vec() };
        }
        if let Some(entry) = self.pending.get(&header.message)
            && (entry.kind != header.kind || entry.parts.len() != usize::from(header.parts))
        {
            self.remove(header.message);
            return Feed::Dropped(DropReason::Mismatch);
        }
        if self.bytes + data.len() > self.limits.max_bytes {
            self.remove(header.message);
            return Feed::Dropped(DropReason::OverBudget);
        }
        if !self.pending.contains_key(&header.message) {
            while self.pending.len() >= self.limits.max_pending.max(1) {
                let oldest = self.pending.iter().min_by_key(|(_, p)| p.started_ms).map(|(id, _)| *id);
                match oldest {
                    Some(id) => self.remove(id),
                    None => break,
                }
            }
            self.pending.insert(
                header.message,
                Pending {
                    kind: header.kind,
                    parts: vec![None; usize::from(header.parts)],
                    received: 0,
                    bytes: 0,
                    started_ms: now_ms,
                },
            );
        }
        let Some(entry) = self.pending.get_mut(&header.message) else { return Feed::Partial };
        let slot = &mut entry.parts[usize::from(header.part)];
        if slot.is_some() {
            self.remove(header.message);
            return Feed::Dropped(DropReason::Duplicate);
        }
        *slot = Some(data.to_vec());
        entry.received += 1;
        entry.bytes += data.len();
        self.bytes += data.len();
        if entry.received < header.parts {
            return Feed::Partial;
        }
        let Some(done) = self.pending.remove(&header.message) else { return Feed::Partial };
        self.bytes -= done.bytes;
        let mut body = Vec::with_capacity(done.bytes);
        for part in done.parts.into_iter().flatten() {
            body.extend_from_slice(&part);
        }
        Feed::Complete { kind: done.kind, body }
    }

    /// Drops unfinished messages older than the timeout.
    pub fn expire(&mut self, now_ms: u64) {
        let timeout = self.limits.timeout_ms;
        let stale: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.started_ms) > timeout)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.remove(id);
        }
    }

    fn remove(&mut self, message: u32) {
        if let Some(entry) = self.pending.remove(&message) {
            self.bytes -= entry.bytes;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KickReason {
    /// No `Hello` in time: most likely a client without the mod.
    NoHello,
    /// The client speaks another protocol version.
    Protocol { client: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Probe(u128),
    Kick(u128, KickReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelloOutcome {
    Welcome { features: u64 },
    Reject(KickReason),
}

#[derive(Clone, Copy)]
enum Admission {
    Waiting { joined_ms: u64, probed_ms: Option<u64> },
    Admitted { features: u64 },
    Kicked,
}

/// Server side of ADR-007: a player must say `Hello` with our protocol soon after joining, or is kicked. Guards against
/// a mistake (a client without the mod), not an attack: a client can claim anything.
pub struct AdmissionGate {
    pub deadline_ms: u64,
    pub probe_every_ms: u64,
    pub server_features: u64,
    players: HashMap<u128, Admission>,
}

impl AdmissionGate {
    pub fn new(server_features: u64) -> Self {
        AdmissionGate { deadline_ms: 10_000, probe_every_ms: 2_000, server_features, players: HashMap::new() }
    }

    /// A player joined: they will be probed now and until they answer.
    pub fn join(&mut self, player: u128, now_ms: u64) {
        self.players.entry(player).or_insert(Admission::Waiting { joined_ms: now_ms, probed_ms: None });
    }

    pub fn leave(&mut self, player: u128) {
        self.players.remove(&player);
    }

    // For the map modules of later phases.
    #[allow(dead_code)]
    pub fn features(&self, player: u128) -> Option<u64> {
        match self.players.get(&player) {
            Some(Admission::Admitted { features }) => Some(*features),
            _ => None,
        }
    }

    pub fn hello(&mut self, player: u128, protocol: u16, client_features: u64) -> HelloOutcome {
        if matches!(self.players.get(&player), Some(Admission::Kicked)) {
            return HelloOutcome::Reject(KickReason::NoHello);
        }
        if protocol != PROTOCOL {
            self.players.insert(player, Admission::Kicked);
            return HelloOutcome::Reject(KickReason::Protocol { client: protocol });
        }
        let features = client_features & self.server_features;
        self.players.insert(player, Admission::Admitted { features });
        HelloOutcome::Welcome { features }
    }

    /// What to do this tick: probes that are due and kicks for players past the deadline.
    pub fn poll(&mut self, now_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        for (player, state) in &mut self.players {
            if let Admission::Waiting { joined_ms, probed_ms } = state {
                if now_ms.saturating_sub(*joined_ms) >= self.deadline_ms {
                    actions.push(Action::Kick(*player, KickReason::NoHello));
                    *state = Admission::Kicked;
                } else if probed_ms.is_none_or(|at| now_ms.saturating_sub(at) >= self.probe_every_ms) {
                    actions.push(Action::Probe(*player));
                    *probed_ms = Some(now_ms);
                }
            }
        }
        actions
    }
}

/// Client side of the handshake. A client cannot tell when a proxy moves it to another server, so it repeats `Hello`
/// until welcomed and answers every `Probe` at once.
pub struct ClientHandshake {
    pub repeat_ms: u64,
    welcomed: bool,
    last_hello_ms: Option<u64>,
}

impl Default for ClientHandshake {
    fn default() -> Self {
        ClientHandshake { repeat_ms: 3_000, welcomed: false, last_hello_ms: None }
    }
}

impl ClientHandshake {
    /// Whether to send `minecraft:register` and `Hello` now.
    pub fn due(&mut self, now_ms: u64) -> bool {
        if self.welcomed || self.last_hello_ms.is_some_and(|at| now_ms.saturating_sub(at) < self.repeat_ms) {
            return false;
        }
        self.last_hello_ms = Some(now_ms);
        true
    }

    /// A new server asks who we are: forget the old welcome and answer now.
    pub fn probed(&mut self) {
        self.welcomed = false;
        self.last_hello_ms = None;
    }

    pub fn welcomed(&mut self) {
        self.welcomed = true;
    }

    // For the map modules of later phases.
    #[allow(dead_code)]
    pub fn is_welcomed(&self) -> bool {
        self.welcomed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: Message, max_frame: usize) -> Message {
        let frames = Outbox::default().frames(&message, max_frame).unwrap();
        let mut reassembler = Reassembler::new(Limits::default());
        let mut done = None;
        for frame in &frames {
            assert!(frame.len() <= max_frame);
            if let Feed::Complete { kind, body } = reassembler.feed(frame, 0) {
                done = Some(Message::decode(kind, &body).unwrap());
            }
        }
        assert_eq!(reassembler.held_bytes(), 0);
        done.unwrap()
    }

    #[test]
    fn every_message_roundtrips() {
        for message in [
            Message::Hello { protocol: PROTOCOL, features: 0b1011, mod_version: "0.1.0".into() },
            Message::Welcome { protocol: PROTOCOL, features: 3 },
            Message::Reject { reason: "Установите мод VexMap".into() },
            Message::Ping(42),
            Message::Pong(u64::MAX),
            Message::Probe,
        ] {
            assert_eq!(roundtrip(message.clone(), MAX_C2S_FRAME), message);
        }
    }

    #[test]
    fn long_strings_are_cut_on_a_char_boundary() {
        let reason = "я".repeat(400);
        let Message::Reject { reason: got } = roundtrip(Message::Reject { reason }, MAX_S2C_FRAME) else { panic!() };
        assert!(got.len() <= MAX_REASON && got.chars().all(|c| c == 'я'));
    }

    #[test]
    fn large_body_splits_and_reassembles_out_of_order() {
        let body: Vec<u8> = (0..100_000u32).map(|i| (i * 7) as u8).collect();
        let mut frames = encode(Kind::Ping, 9, &body, 1000).unwrap();
        assert_eq!(frames.len(), 102);
        frames.reverse();
        frames.swap(3, 50);
        let mut reassembler = Reassembler::new(Limits::default());
        let mut result = None;
        for frame in &frames {
            match reassembler.feed(frame, 0) {
                Feed::Partial => {}
                Feed::Complete { body, .. } => result = Some(body),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(result.unwrap(), body);
        assert_eq!(reassembler.pending(), 0);
    }

    #[test]
    fn empty_body_is_one_frame() {
        let frames = encode(Kind::Probe, 0, &[], MAX_C2S_FRAME).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), HEADER_LEN);
    }

    #[test]
    fn too_many_parts_is_refused() {
        assert_eq!(encode(Kind::Ping, 0, &vec![0; 4097], HEADER_LEN + 1), Err(FrameError::TooLarge));
        assert_eq!(encode(Kind::Ping, 0, &[], HEADER_LEN), Err(FrameError::TooLarge));
    }

    #[test]
    fn garbage_never_panics() {
        let mut reassembler = Reassembler::new(Limits::default());
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        for len in 0..2000 {
            let mut frame: Vec<u8> = (0..len % 64)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            if len % 3 == 0 && frame.len() >= HEADER_LEN {
                frame[..3].copy_from_slice(&[b'V', b'M', FRAME_VERSION]);
            }
            if let Feed::Complete { kind, body } = reassembler.feed(&frame, len as u64) {
                let _ = Message::decode(kind, &body);
            }
            assert!(reassembler.pending() <= 8);
            assert!(reassembler.held_bytes() <= 4 << 20);
        }
    }

    #[test]
    fn bad_headers_are_rejected() {
        let good = encode(Kind::Ping, 1, &[0; 8], 100).unwrap().remove(0);
        assert_eq!(parse_frame(&good[..5]).unwrap_err(), FrameError::TooShort);
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(parse_frame(&bad).unwrap_err(), FrameError::BadMagic);
        let mut bad = good.clone();
        bad[2] = 9;
        assert_eq!(parse_frame(&bad).unwrap_err(), FrameError::BadVersion(9));
        let mut bad = good.clone();
        bad[8] = 1; // part 1 of 1
        assert_eq!(parse_frame(&bad).unwrap_err(), FrameError::BadParts);
        let mut bad = good;
        bad[10..12].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(parse_frame(&bad).unwrap_err(), FrameError::BadParts);
    }

    #[test]
    fn duplicate_and_mismatched_parts_drop_the_message() {
        let frames = encode(Kind::Ping, 5, &[1; 300], 100).unwrap();
        let mut reassembler = Reassembler::new(Limits::default());
        assert_eq!(reassembler.feed(&frames[0], 0), Feed::Partial);
        assert_eq!(reassembler.feed(&frames[0], 0), Feed::Dropped(DropReason::Duplicate));
        assert_eq!(reassembler.held_bytes(), 0);

        let other = encode(Kind::Pong, 5, &[1; 300], 100).unwrap();
        assert_eq!(reassembler.feed(&frames[0], 0), Feed::Partial);
        assert_eq!(reassembler.feed(&other[1], 0), Feed::Dropped(DropReason::Mismatch));
        assert_eq!(reassembler.pending(), 0);
    }

    #[test]
    fn budget_and_pending_limits_hold() {
        let limits = Limits { max_pending: 2, max_bytes: 500, timeout_ms: 1000 };
        let mut reassembler = Reassembler::new(limits);
        for message in 0..5 {
            let frames = encode(Kind::Ping, message, &[0; 200], 100).unwrap();
            reassembler.feed(&frames[0], u64::from(message));
            assert!(reassembler.pending() <= 2);
        }
        let big = encode(Kind::Ping, 99, &[0; 2000], 600).unwrap();
        reassembler.feed(&big[0], 10);
        assert_eq!(reassembler.feed(&big[1], 10), Feed::Dropped(DropReason::OverBudget));
        assert!(reassembler.held_bytes() <= 500);
    }

    #[test]
    fn unfinished_messages_expire() {
        let frames = encode(Kind::Ping, 1, &[0; 300], 100).unwrap();
        let mut reassembler = Reassembler::new(Limits { timeout_ms: 1000, ..Limits::default() });
        reassembler.feed(&frames[0], 0);
        reassembler.expire(1001);
        assert_eq!(reassembler.pending(), 0);
        assert_eq!(reassembler.held_bytes(), 0);
    }

    #[test]
    fn decode_rejects_bad_bodies() {
        assert_eq!(Message::decode(99, &[]), Err(DecodeError::UnknownKind(99)));
        assert_eq!(Message::decode(Kind::Ping as u8, &[0; 7]), Err(DecodeError::Truncated));
        assert_eq!(Message::decode(Kind::Ping as u8, &[0; 9]), Err(DecodeError::TrailingBytes));
        assert_eq!(Message::decode(Kind::Reject as u8, &[2, 0, 0xFF, 0xFE]), Err(DecodeError::BadString));
        assert_eq!(Message::decode(Kind::Reject as u8, &[0xFF, 0xFF]), Err(DecodeError::BadString));
    }

    #[test]
    fn gate_admits_a_good_hello() {
        let mut gate = AdmissionGate::new(0b0110);
        gate.join(1, 0);
        assert_eq!(gate.poll(0), vec![Action::Probe(1)]);
        assert_eq!(gate.poll(1000), vec![]);
        assert_eq!(gate.poll(2000), vec![Action::Probe(1)]);
        assert_eq!(gate.hello(1, PROTOCOL, 0b0011), HelloOutcome::Welcome { features: 0b0010 });
        assert_eq!(gate.features(1), Some(0b0010));
        assert_eq!(gate.poll(60_000), vec![]);
    }

    #[test]
    fn gate_kicks_silence_once() {
        let mut gate = AdmissionGate::new(0);
        gate.join(7, 100);
        gate.poll(100);
        assert_eq!(gate.poll(10_100), vec![Action::Kick(7, KickReason::NoHello)]);
        assert_eq!(gate.poll(20_000), vec![]);
        assert_eq!(gate.hello(7, PROTOCOL, 0), HelloOutcome::Reject(KickReason::NoHello));
        gate.leave(7);
        gate.join(7, 30_000);
        assert_eq!(gate.poll(30_000), vec![Action::Probe(7)]);
    }

    #[test]
    fn gate_rejects_another_protocol() {
        let mut gate = AdmissionGate::new(0);
        gate.join(2, 0);
        assert_eq!(gate.hello(2, PROTOCOL + 1, 0), HelloOutcome::Reject(KickReason::Protocol { client: PROTOCOL + 1 }));
        assert_eq!(gate.features(2), None);
        assert_eq!(gate.poll(50_000), vec![]);
    }

    #[test]
    fn hello_before_join_is_kept() {
        let mut gate = AdmissionGate::new(1);
        assert_eq!(gate.hello(3, PROTOCOL, 1), HelloOutcome::Welcome { features: 1 });
        gate.join(3, 0);
        assert_eq!(gate.poll(20_000), vec![]);
        assert_eq!(gate.features(3), Some(1));
    }

    #[test]
    fn client_repeats_until_welcomed_and_answers_probes() {
        let mut client = ClientHandshake::default();
        assert!(client.due(0));
        assert!(!client.due(2999));
        assert!(client.due(3000));
        client.welcomed();
        assert!(!client.due(10_000));
        client.probed();
        assert!(!client.is_welcomed());
        assert!(client.due(10_001));
    }
}
