// VexMap wire protocol: frames, splitting, reassembly, handshake and admission. Knows nothing of Horizon and uses only std,
// so the same file is the `vexmap-proto` crate (tests, the future sidecar) and the `vexmap_proto` module injected into the
// game. Module payloads cannot carry inner attributes or `mod` files: keep this one self-contained file.
//
// Everything a client sends is untrusted input. Nothing here panics on bad bytes, and every buffer is bounded.

use std::collections::HashMap;

pub const CHANNEL: &str = "vexmap:main";
pub const PROTOCOL: u16 = 2;
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
/// The most markers one key frame carries. At about 47 bytes each it stays inside one `MAX_S2C_FRAME`.
pub const MAX_MARKERS: usize = 8192;
const MAX_MARKER_NAME: usize = 16;
const MAX_DIMENSION: usize = 64;
const MAX_DIMENSIONS: usize = 255;
/// A client reads a handful of streams, so this only bounds what a confused server could make it hold.
pub const MAX_STREAMS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hello = 1,
    Welcome = 2,
    Reject = 3,
    Ping = 4,
    Pong = 5,
    Probe = 6,
    PositionKey = 7,
    PositionDelta = 8,
    PositionStreams = 9,
    PositionClusters = 10,
    OpenMap = 11,
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
            7 => Kind::PositionKey,
            8 => Kind::PositionDelta,
            9 => Kind::PositionStreams,
            10 => Kind::PositionClusters,
            11 => Kind::OpenMap,
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
    Hello {
        protocol: u16,
        features: u64,
        mod_version: String,
    },
    Welcome {
        protocol: u16,
        features: u64,
        /// The client's own UUID as the server knows it, so the map can leave the player's own marker out.
        player: u128,
    },
    Reject {
        reason: String,
    },
    Ping(u64),
    Pong(u64),
    Probe,
    PositionKey(PositionKey),
    PositionDelta(PositionDelta),
    /// The streams this client should hold. Any other is dropped, so an empty list clears the markers.
    PositionStreams(Vec<u32>),
    /// Everything the client has no markers for, summed up. Replaces the previous list.
    PositionClusters(Vec<Cluster>),
    /// The player ran `/map`. Commands live on the server, the screen on the client.
    OpenMap,
}

/// A player on the map. Coordinates in 1/8 of a block, `yaw` in 1/256 of a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub uuid: u128,
    pub name: String,
    pub dimension: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub yaw: u8,
}

/// A group of players too far away or too many to show one by one. `x` and `z` are their centre, in 1/8 of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub dimension: String,
    pub x: i32,
    pub z: i32,
    pub count: u16,
}

/// A whole stream. Replaces what the client held for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionKey {
    pub stream: u32,
    pub key: u32,
    pub markers: Vec<Marker>,
}

/// One marker's change against its entry in the key frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerMove {
    pub index: u16,
    pub dx: i32,
    pub dy: i32,
    pub dz: i32,
    pub yaw: u8,
}

/// The channel now, as a difference from key frame `key`. Never from the previous delta: a lost or dropped delta costs
/// nothing, because the next one is complete on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PositionDelta {
    pub stream: u32,
    pub key: u32,
    pub seq: u32,
    pub removed: Vec<u16>,
    pub added: Vec<Marker>,
    pub moved: Vec<MarkerMove>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    UnknownKind(u8),
    Truncated,
    TrailingBytes,
    BadString,
    BadIndex,
    TooMany,
    BadNumber,
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
            Message::PositionKey(_) => Kind::PositionKey,
            Message::PositionDelta(_) => Kind::PositionDelta,
            Message::PositionStreams(_) => Kind::PositionStreams,
            Message::PositionClusters(_) => Kind::PositionClusters,
            Message::OpenMap => Kind::OpenMap,
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
            Message::Welcome { protocol, features, player } => {
                out.extend_from_slice(&protocol.to_le_bytes());
                out.extend_from_slice(&features.to_le_bytes());
                out.extend_from_slice(&player.to_le_bytes());
            }
            Message::Reject { reason } => put_str(&mut out, reason, MAX_REASON),
            Message::Ping(stamp) | Message::Pong(stamp) => out.extend_from_slice(&stamp.to_le_bytes()),
            Message::Probe | Message::OpenMap => {}
            Message::PositionKey(frame) => {
                out.extend_from_slice(&frame.stream.to_le_bytes());
                out.extend_from_slice(&frame.key.to_le_bytes());
                put_markers(&mut out, &frame.markers);
            }
            Message::PositionStreams(streams) => {
                out.extend_from_slice(&(streams.len().min(MAX_STREAMS) as u16).to_le_bytes());
                for stream in streams.iter().take(MAX_STREAMS) {
                    out.extend_from_slice(&stream.to_le_bytes());
                }
            }
            Message::PositionClusters(clusters) => put_clusters(&mut out, clusters),
            Message::PositionDelta(delta) => {
                out.extend_from_slice(&delta.stream.to_le_bytes());
                out.extend_from_slice(&delta.key.to_le_bytes());
                out.extend_from_slice(&delta.seq.to_le_bytes());
                out.extend_from_slice(&(delta.removed.len() as u16).to_le_bytes());
                for index in &delta.removed {
                    out.extend_from_slice(&index.to_le_bytes());
                }
                put_markers(&mut out, &delta.added);
                out.extend_from_slice(&(delta.moved.len() as u16).to_le_bytes());
                for moved in &delta.moved {
                    out.extend_from_slice(&moved.index.to_le_bytes());
                    put_varint(&mut out, moved.dx);
                    put_varint(&mut out, moved.dy);
                    put_varint(&mut out, moved.dz);
                    out.push(moved.yaw);
                }
            }
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
            Kind::Welcome => {
                Message::Welcome { protocol: reader.u16()?, features: reader.u64()?, player: reader.u128()? }
            }
            Kind::Reject => Message::Reject { reason: reader.string(MAX_REASON)? },
            Kind::Ping => Message::Ping(reader.u64()?),
            Kind::Pong => Message::Pong(reader.u64()?),
            Kind::Probe => Message::Probe,
            Kind::OpenMap => Message::OpenMap,
            Kind::PositionKey => Message::PositionKey(PositionKey {
                stream: reader.u32()?,
                key: reader.u32()?,
                markers: reader.markers()?,
            }),
            Kind::PositionStreams => Message::PositionStreams(reader.streams()?),
            Kind::PositionClusters => Message::PositionClusters(reader.clusters()?),
            Kind::PositionDelta => Message::PositionDelta(reader.delta()?),
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

fn put_short(out: &mut Vec<u8>, text: &str) {
    out.push(text.len() as u8);
    out.extend_from_slice(text.as_bytes());
}

/// A table of dimensions, then the markers pointing into it. A marker this format cannot hold is left out.
fn put_markers(out: &mut Vec<u8>, markers: &[Marker]) {
    let mut dimensions: Vec<&str> = Vec::new();
    let mut entries = Vec::with_capacity(markers.len().min(MAX_MARKERS) * 40);
    let mut count: u16 = 0;
    for marker in markers {
        if usize::from(count) == MAX_MARKERS
            || marker.name.len() > MAX_MARKER_NAME
            || marker.dimension.len() > MAX_DIMENSION
        {
            continue;
        }
        let index = match dimensions.iter().position(|known| *known == marker.dimension) {
            Some(index) => index,
            None if dimensions.len() < MAX_DIMENSIONS => {
                dimensions.push(&marker.dimension);
                dimensions.len() - 1
            }
            None => continue,
        };
        entries.extend_from_slice(&marker.uuid.to_le_bytes());
        put_short(&mut entries, &marker.name);
        entries.push(index as u8);
        entries.extend_from_slice(&marker.x.to_le_bytes());
        entries.extend_from_slice(&marker.y.to_le_bytes());
        entries.extend_from_slice(&marker.z.to_le_bytes());
        entries.push(marker.yaw);
        count += 1;
    }
    out.push(dimensions.len() as u8);
    for dimension in &dimensions {
        put_short(out, dimension);
    }
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&entries);
}

/// A table of dimensions, then the clusters pointing into it.
fn put_clusters(out: &mut Vec<u8>, clusters: &[Cluster]) {
    let mut dimensions: Vec<&str> = Vec::new();
    let mut entries = Vec::with_capacity(clusters.len().min(MAX_MARKERS) * 11);
    let mut count: u16 = 0;
    for cluster in clusters {
        if usize::from(count) == MAX_MARKERS || cluster.dimension.len() > MAX_DIMENSION {
            continue;
        }
        let index = match dimensions.iter().position(|known| *known == cluster.dimension) {
            Some(index) => index,
            None if dimensions.len() < MAX_DIMENSIONS => {
                dimensions.push(&cluster.dimension);
                dimensions.len() - 1
            }
            None => continue,
        };
        entries.push(index as u8);
        entries.extend_from_slice(&cluster.x.to_le_bytes());
        entries.extend_from_slice(&cluster.z.to_le_bytes());
        entries.extend_from_slice(&cluster.count.to_le_bytes());
        count += 1;
    }
    out.push(dimensions.len() as u8);
    for dimension in &dimensions {
        put_short(out, dimension);
    }
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&entries);
}

/// Zigzag, seven bits a byte: a walking player's offset from the key frame takes one or two bytes.
fn put_varint(out: &mut Vec<u8>, value: i32) {
    let mut rest = ((value << 1) ^ (value >> 31)) as u32;
    while rest >= 0x80 {
        out.push(rest as u8 | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
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

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u128(&mut self) -> Result<u128, DecodeError> {
        let mut b = [0; 16];
        b.copy_from_slice(self.take(16)?);
        Ok(u128::from_le_bytes(b))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(self.u32()? as i32)
    }

    fn short(&mut self, max: usize) -> Result<String, DecodeError> {
        let len = usize::from(self.u8()?);
        if len > max {
            return Err(DecodeError::BadString);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| DecodeError::BadString)
    }

    fn count(&mut self) -> Result<usize, DecodeError> {
        let count = usize::from(self.u16()?);
        if count > MAX_MARKERS {
            return Err(DecodeError::TooMany);
        }
        Ok(count)
    }

    fn varint(&mut self) -> Result<i32, DecodeError> {
        let mut value = 0u32;
        for shift in (0..35).step_by(7) {
            let byte = self.u8()?;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok((value >> 1) as i32 ^ -((value & 1) as i32));
            }
        }
        Err(DecodeError::BadNumber)
    }

    fn markers(&mut self) -> Result<Vec<Marker>, DecodeError> {
        let mut dimensions = Vec::new();
        for _ in 0..self.u8()? {
            dimensions.push(self.short(MAX_DIMENSION)?);
        }
        let count = self.count()?;
        let mut markers = Vec::with_capacity(count);
        for _ in 0..count {
            let mut uuid = [0; 16];
            uuid.copy_from_slice(self.take(16)?);
            let name = self.short(MAX_MARKER_NAME)?;
            let dimension = dimensions.get(usize::from(self.u8()?)).ok_or(DecodeError::BadIndex)?.clone();
            let (x, y, z) = (self.i32()?, self.i32()?, self.i32()?);
            markers.push(Marker { uuid: u128::from_le_bytes(uuid), name, dimension, x, y, z, yaw: self.u8()? });
        }
        Ok(markers)
    }

    fn streams(&mut self) -> Result<Vec<u32>, DecodeError> {
        let count = usize::from(self.u16()?);
        if count > MAX_STREAMS {
            return Err(DecodeError::TooMany);
        }
        (0..count).map(|_| self.u32()).collect()
    }

    fn clusters(&mut self) -> Result<Vec<Cluster>, DecodeError> {
        let mut dimensions = Vec::new();
        for _ in 0..self.u8()? {
            dimensions.push(self.short(MAX_DIMENSION)?);
        }
        let count = self.count()?;
        let mut clusters = Vec::with_capacity(count);
        for _ in 0..count {
            let dimension = dimensions.get(usize::from(self.u8()?)).ok_or(DecodeError::BadIndex)?.clone();
            clusters.push(Cluster { dimension, x: self.i32()?, z: self.i32()?, count: self.u16()? });
        }
        Ok(clusters)
    }

    fn delta(&mut self) -> Result<PositionDelta, DecodeError> {
        let (stream, key, seq) = (self.u32()?, self.u32()?, self.u32()?);
        let removed_count = self.count()?;
        let mut removed = Vec::with_capacity(removed_count);
        for _ in 0..removed_count {
            removed.push(self.u16()?);
        }
        let added = self.markers()?;
        let moved_count = self.count()?;
        let mut moved = Vec::with_capacity(moved_count);
        for _ in 0..moved_count {
            let index = self.u16()?;
            let (dx, dy, dz) = (self.varint()?, self.varint()?, self.varint()?);
            moved.push(MarkerMove { index, dx, dy, dz, yaw: self.u8()? });
        }
        Ok(PositionDelta { stream, key, seq, removed, added, moved })
    }
}

/// The sidecar's half of a position channel: remembers the key frame and describes the present against it.
// Used by the sidecar and tests, not by the game.
#[allow(dead_code)]
#[derive(Default)]
pub struct ChannelEncoder {
    stream: u32,
    key: u32,
    seq: u32,
    base: Vec<Marker>,
    index: HashMap<u128, u16>,
    last: Vec<Marker>,
    delta: PositionDelta,
}

#[allow(dead_code)]
impl ChannelEncoder {
    /// `markers` becomes the new key frame of `stream`, numbered `key`.
    pub fn key_frame(&mut self, stream: u32, key: u32, mut markers: Vec<Marker>) -> PositionKey {
        markers.truncate(MAX_MARKERS);
        self.stream = stream;
        self.key = key;
        self.seq = 0;
        self.index = markers.iter().enumerate().map(|(index, marker)| (marker.uuid, index as u16)).collect();
        self.base.clone_from(&markers);
        self.last.clone_from(&markers);
        self.delta = PositionDelta { stream, key, ..PositionDelta::default() };
        PositionKey { stream, key, markers }
    }

    /// `markers` against the key frame, and whether anything changed since the previous call. An unchanged channel
    /// repeats its latest delta, which a viewer joining now still needs after the key frame.
    pub fn delta(&mut self, mut markers: Vec<Marker>) -> (&PositionDelta, bool) {
        markers.truncate(MAX_MARKERS);
        if markers == self.last {
            return (&self.delta, false);
        }
        self.seq = self.seq.wrapping_add(1);
        let mut delta = PositionDelta { stream: self.stream, key: self.key, seq: self.seq, ..PositionDelta::default() };
        let mut kept = vec![false; self.base.len()];
        for marker in &markers {
            let known = self.index.get(&marker.uuid).map(|index| (*index, &self.base[usize::from(*index)]));
            match known {
                Some((index, base)) if base.dimension == marker.dimension && base.name == marker.name => {
                    kept[usize::from(index)] = true;
                    if (base.x, base.y, base.z, base.yaw) != (marker.x, marker.y, marker.z, marker.yaw) {
                        delta.moved.push(MarkerMove {
                            index,
                            dx: marker.x.wrapping_sub(base.x),
                            dy: marker.y.wrapping_sub(base.y),
                            dz: marker.z.wrapping_sub(base.z),
                            yaw: marker.yaw,
                        });
                    }
                }
                _ => delta.added.push(marker.clone()),
            }
        }
        delta.removed = (0..self.base.len()).filter(|index| !kept[*index]).map(|index| index as u16).collect();
        self.last = markers;
        self.delta = delta;
        (&self.delta, true)
    }
}

/// The client's half of a position channel.
#[derive(Default)]
pub struct PositionTable {
    key: Option<u32>,
    seq: u32,
    base: Vec<Marker>,
    current: Vec<Marker>,
}

impl PositionTable {
    pub fn markers(&self) -> &[Marker] {
        &self.current
    }

    pub fn apply_key(&mut self, frame: PositionKey) {
        self.key = Some(frame.key);
        self.seq = 0;
        self.current.clone_from(&frame.markers);
        self.base = frame.markers;
    }

    /// `false` for a delta that does not apply: another key frame, older than what is shown, or malformed.
    pub fn apply_delta(&mut self, delta: &PositionDelta) -> bool {
        if self.key != Some(delta.key) || delta.seq <= self.seq {
            return false;
        }
        let in_range = |index: u16| usize::from(index) < self.base.len();
        if !delta.removed.iter().all(|index| in_range(*index)) || !delta.moved.iter().all(|m| in_range(m.index)) {
            return false;
        }
        self.seq = delta.seq;
        let mut next = self.base.clone();
        for moved in &delta.moved {
            let marker = &mut next[usize::from(moved.index)];
            marker.x = marker.x.wrapping_add(moved.dx);
            marker.y = marker.y.wrapping_add(moved.dy);
            marker.z = marker.z.wrapping_add(moved.dz);
            marker.yaw = moved.yaw;
        }
        let mut gone = vec![false; next.len()];
        for index in &delta.removed {
            gone[usize::from(*index)] = true;
        }
        let mut flags = gone.iter();
        next.retain(|_| !flags.next().copied().unwrap_or(false));
        next.extend_from_slice(&delta.added);
        self.current = next;
        true
    }
}

/// Everything a client knows about other players: one table per stream, and the clusters.
#[derive(Default)]
pub struct PositionTables {
    streams: HashMap<u32, PositionTable>,
    clusters: Vec<Cluster>,
}

impl PositionTables {
    pub fn marker_count(&self) -> usize {
        self.streams.values().map(|table| table.markers().len()).sum()
    }

    // Read by the map once it is drawn, and by tests.
    #[allow(dead_code)]
    pub fn markers(&self) -> impl Iterator<Item = &Marker> {
        self.streams.values().flat_map(|table| table.markers())
    }

    // Read by the map once it is drawn, and by tests.
    #[allow(dead_code)]
    pub fn clusters(&self) -> &[Cluster] {
        &self.clusters
    }

    /// `false` when it was ignored or changed nothing.
    pub fn apply(&mut self, message: Message) -> bool {
        match message {
            Message::PositionKey(frame) => {
                if self.streams.len() >= MAX_STREAMS && !self.streams.contains_key(&frame.stream) {
                    return false;
                }
                self.streams.entry(frame.stream).or_default().apply_key(frame);
                true
            }
            Message::PositionDelta(delta) => {
                self.streams.get_mut(&delta.stream).is_some_and(|table| table.apply_delta(&delta))
            }
            Message::PositionStreams(streams) => {
                self.streams.retain(|stream, _| streams.contains(stream));
                true
            }
            Message::PositionClusters(clusters) => {
                self.clusters = clusters;
                true
            }
            _ => false,
        }
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
    /// The required resource pack was not applied (ADR-008).
    Pack(PackFailure),
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

    /// `Some` once the player is admitted.
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

/// How a required pack failed, which decides what the player is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackFailure {
    Declined,
    /// Download, an unusable address or the reload failed: not the player's choice.
    Failed,
    /// No final answer in time.
    TimedOut,
}

/// `ServerboundResourcePackPacket.Action`, as `native_resource_pack_status` numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackStatus {
    Loaded,
    Declined,
    Accepted,
    Downloaded,
    Failed,
    Discarded,
}

impl PackStatus {
    pub fn from_action(action: i32) -> Option<PackStatus> {
        Some(match action {
            0 => PackStatus::Loaded,
            1 => PackStatus::Declined,
            2 | 5 | 6 => PackStatus::Failed,
            3 => PackStatus::Accepted,
            4 => PackStatus::Downloaded,
            7 => PackStatus::Discarded,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy)]
enum PackProgress {
    Waiting { deadline_ms: u64 },
    Done,
}

/// Server side of decision 133: every admitted player must apply the pack, or is told why and disconnected. Each region
/// server asks again, because a proxy hides from the client which server it talks to.
pub struct PackGate {
    /// Time to answer the prompt.
    pub answer_ms: u64,
    /// Time to download and apply once accepted.
    pub apply_ms: u64,
    players: HashMap<u128, PackProgress>,
}

impl Default for PackGate {
    fn default() -> Self {
        PackGate { answer_ms: 120_000, apply_ms: 120_000, players: HashMap::new() }
    }
}

impl PackGate {
    /// `true` the first time for this player on this server: send the pack now. `Hello` repeats, the pack must not.
    pub fn offer(&mut self, player: u128, now_ms: u64) -> bool {
        if self.players.contains_key(&player) {
            return false;
        }
        self.players.insert(player, PackProgress::Waiting { deadline_ms: now_ms + self.answer_ms });
        true
    }

    pub fn leave(&mut self, player: u128) {
        self.players.remove(&player);
    }

    /// The client's answer. `Some` means disconnect. Answers from someone we are not waiting on change nothing.
    pub fn status(&mut self, player: u128, status: PackStatus, now_ms: u64) -> Option<PackFailure> {
        let progress = self.players.get_mut(&player)?;
        if !matches!(progress, PackProgress::Waiting { .. }) {
            return None;
        }
        match status {
            PackStatus::Loaded => *progress = PackProgress::Done,
            PackStatus::Accepted | PackStatus::Downloaded => {
                *progress = PackProgress::Waiting { deadline_ms: now_ms + self.apply_ms };
            }
            PackStatus::Discarded => {}
            PackStatus::Declined | PackStatus::Failed => {
                *progress = PackProgress::Done;
                return Some(if status == PackStatus::Declined { PackFailure::Declined } else { PackFailure::Failed });
            }
        }
        None
    }

    /// Players past their deadline. Each is returned once.
    pub fn poll(&mut self, now_ms: u64) -> Vec<u128> {
        let mut late = Vec::new();
        for (player, progress) in &mut self.players {
            if matches!(progress, PackProgress::Waiting { deadline_ms } if now_ms >= *deadline_ms) {
                *progress = PackProgress::Done;
                late.push(*player);
            }
        }
        late
    }
}

/// How long the mod holds the server tick. Fixed memory, so measuring costs next to nothing.
pub struct TickMeter {
    samples: Vec<u32>,
    filled: usize,
    scratch: Vec<u32>,
}

/// One report of [`TickMeter`], in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickReport {
    pub ticks: usize,
    pub p50_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
}

impl TickMeter {
    /// `window` ticks per report.
    pub fn new(window: usize) -> Self {
        TickMeter { samples: vec![0; window.max(1)], filled: 0, scratch: Vec::with_capacity(window.max(1)) }
    }

    /// `Some` every `window` ticks.
    pub fn record(&mut self, nanos: u64) -> Option<TickReport> {
        let slot = self.samples.get_mut(self.filled)?;
        *slot = nanos.min(u64::from(u32::MAX)) as u32;
        self.filled += 1;
        if self.filled < self.samples.len() {
            return None;
        }
        self.filled = 0;
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.samples);
        self.scratch.sort_unstable();
        let at = |fraction: f64| {
            let index = ((self.scratch.len() - 1) as f64 * fraction).round() as usize;
            self.scratch.get(index).copied().unwrap_or(0) / 1000
        };
        Some(TickReport { ticks: self.scratch.len(), p50_us: at(0.5), p99_us: at(0.99), max_us: at(1.0) })
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
            Message::Welcome { protocol: PROTOCOL, features: 3, player: u128::MAX - 7 },
            Message::OpenMap,
            Message::Reject { reason: "Установите мод VexMap".into() },
            Message::Ping(42),
            Message::Pong(u64::MAX),
            Message::Probe,
        ] {
            assert_eq!(roundtrip(message.clone(), MAX_C2S_FRAME), message);
        }
    }

    fn marker(uuid: u128, x: i32) -> Marker {
        Marker { uuid, name: format!("P{uuid}"), dimension: "minecraft:overworld".into(), x, y: 512, z: -x, yaw: 7 }
    }

    fn sorted(markers: &[Marker]) -> Vec<Marker> {
        let mut out = markers.to_vec();
        out.sort_by_key(|marker| marker.uuid);
        out
    }

    #[test]
    fn position_messages_roundtrip() {
        let key = Message::PositionKey(PositionKey {
            stream: 0,
            key: 9,
            markers: vec![marker(1, -239_999_872), marker(2, 80)],
        });
        assert_eq!(roundtrip(key.clone(), MAX_S2C_FRAME), key);
        let delta = Message::PositionDelta(PositionDelta {
            stream: 0,
            key: 9,
            seq: 3,
            removed: vec![1],
            added: vec![marker(5, 1)],
            moved: vec![
                MarkerMove { index: 0, dx: -1, dy: 0, dz: 300, yaw: 200 },
                MarkerMove { index: 2, dx: i32::MIN, dy: i32::MAX, dz: 63, yaw: 0 },
            ],
        });
        assert_eq!(roundtrip(delta.clone(), MAX_S2C_FRAME), delta);
    }

    #[test]
    fn a_walking_player_costs_a_handful_of_bytes() {
        let mut encoder = ChannelEncoder::default();
        encoder.key_frame(0, 1, vec![marker(1, 0)]);
        let mut moved = marker(1, 40);
        moved.z = -40;
        let (delta, changed) = encoder.delta(vec![moved]);
        assert!(changed);
        // stream, key, seq, three empty counts and one empty dimension table, then index, three offsets and yaw.
        assert_eq!(Message::PositionDelta(delta.clone()).body().len(), 4 + 4 + 4 + 2 + 1 + 2 + 2 + (2 + 1 + 1 + 1 + 1));
    }

    #[test]
    fn every_truncation_of_a_position_message_is_an_error() {
        let key = Message::PositionKey(PositionKey { stream: 0, key: 1, markers: vec![marker(1, 4)] });
        let delta = Message::PositionDelta(PositionDelta {
            stream: 0,
            key: 1,
            seq: 1,
            removed: vec![0],
            added: vec![marker(2, 2)],
            moved: vec![MarkerMove { index: 0, dx: 1000, dy: 1, dz: 1, yaw: 1 }],
        });
        for message in [key, delta] {
            let body = message.body();
            for cut in 0..body.len() {
                assert!(Message::decode(message.kind() as u8, &body[..cut]).is_err(), "cut at {cut}");
            }
        }
    }

    #[test]
    fn hostile_position_messages_are_refused() {
        let mut too_many = vec![0; 8];
        too_many.extend_from_slice(&[0, 0xff, 0xff]);
        assert_eq!(Message::decode(7, &too_many), Err(DecodeError::TooMany));

        let mut overlong = vec![0; 12];
        overlong.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0, 0, 0]);
        assert_eq!(Message::decode(8, &overlong), Err(DecodeError::BadNumber));
    }

    #[test]
    fn key_plus_delta_is_always_the_truth() {
        let mut encoder = ChannelEncoder::default();
        let mut table = PositionTable::default();
        let mut world: Vec<Marker> = (0..40).map(|id| marker(id, id as i32 * 8)).collect();
        table.apply_key(encoder.key_frame(0, 1, world.clone()));
        let mut random = 0x2545_f491_u32;
        for round in 0..200u128 {
            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            for marker in &mut world {
                marker.x += (random >> 8) as i32 % 50;
                marker.yaw = marker.yaw.wrapping_add(random as u8);
            }
            if round % 7 == 0 {
                world.remove(random as usize % world.len());
            }
            if round % 5 == 0 {
                world.push(marker(1000 + round, 3));
            }
            if round % 11 == 0 {
                let index = random as usize % world.len();
                world[index].dimension = "minecraft:the_nether".into();
            }
            let (delta, changed) = encoder.delta(world.clone());
            assert!(changed);
            // Only every third delta arrives: each one must stand on its own.
            if round % 3 == 0 {
                assert!(table.apply_delta(delta));
                assert_eq!(sorted(table.markers()), sorted(&world), "round {round}");
            }
        }
    }

    #[test]
    fn an_unchanged_channel_says_so() {
        let mut encoder = ChannelEncoder::default();
        encoder.key_frame(0, 1, vec![marker(1, 0)]);
        assert!(!encoder.delta(vec![marker(1, 0)]).1);
        assert!(encoder.delta(vec![marker(1, 8)]).1);
        let (delta, changed) = encoder.delta(vec![marker(1, 8)]);
        assert!(!changed);
        assert_eq!((delta.seq, delta.moved.len()), (1, 1), "a late joiner still gets the latest delta");
    }

    #[test]
    fn deltas_that_do_not_apply_change_nothing() {
        let mut encoder = ChannelEncoder::default();
        let mut table = PositionTable::default();
        let (first, _) = encoder.delta(vec![]);
        assert!(!table.apply_delta(first), "no key frame yet");

        table.apply_key(encoder.key_frame(0, 2, vec![marker(1, 0)]));
        let shown = table.markers().to_vec();
        let other_key = PositionDelta { stream: 0, key: 1, seq: 5, ..PositionDelta::default() };
        assert!(!table.apply_delta(&other_key));
        let bad_index = PositionDelta { stream: 0, key: 2, seq: 5, removed: vec![9], ..PositionDelta::default() };
        assert!(!table.apply_delta(&bad_index));
        assert_eq!(table.markers(), shown);

        let newer = PositionDelta { stream: 0, key: 2, seq: 5, removed: vec![0], ..PositionDelta::default() };
        assert!(table.apply_delta(&newer));
        assert!(table.markers().is_empty());
        let older = PositionDelta { stream: 0, key: 2, seq: 4, ..PositionDelta::default() };
        assert!(!table.apply_delta(&older), "an older delta never undoes a newer one");
    }

    #[test]
    fn stream_lists_and_clusters_roundtrip() {
        let streams = Message::PositionStreams(vec![1, 9, u32::MAX]);
        assert_eq!(roundtrip(streams.clone(), MAX_S2C_FRAME), streams);
        let clusters = Message::PositionClusters(vec![
            Cluster { dimension: "minecraft:overworld".into(), x: -80, z: 16, count: 300 },
            Cluster { dimension: "minecraft:the_end".into(), x: 0, z: 0, count: 1 },
        ]);
        assert_eq!(roundtrip(clusters.clone(), MAX_S2C_FRAME), clusters);
        assert_eq!(Message::decode(9, &[0xff, 0xff]), Err(DecodeError::TooMany));
    }

    #[test]
    fn a_client_holds_only_the_streams_it_was_told_to() {
        let mut tables = PositionTables::default();
        for stream in [1, 2] {
            let markers = vec![marker(u128::from(stream), 0)];
            assert!(tables.apply(Message::PositionKey(PositionKey { stream, key: stream, markers })));
        }
        assert_eq!(tables.marker_count(), 2);
        tables.apply(Message::PositionStreams(vec![2]));
        assert_eq!(tables.markers().map(|m| m.uuid).collect::<Vec<_>>(), [2]);
        let unknown = PositionDelta { stream: 1, key: 1, seq: 1, ..PositionDelta::default() };
        assert!(!tables.apply(Message::PositionDelta(unknown)));
        tables.apply(Message::PositionStreams(vec![]));
        assert_eq!(tables.marker_count(), 0);
    }

    #[test]
    fn a_confused_server_cannot_grow_the_client_without_limit() {
        let mut tables = PositionTables::default();
        for stream in 0..MAX_STREAMS as u32 + 10 {
            tables.apply(Message::PositionKey(PositionKey { stream, key: 1, markers: vec![marker(1, 0)] }));
        }
        assert_eq!(tables.marker_count(), MAX_STREAMS);
    }

    #[test]
    fn an_empty_key_frame_clears_the_map() {
        let mut table = PositionTable::default();
        table.apply_key(PositionKey { stream: 0, key: 1, markers: vec![marker(1, 0)] });
        table.apply_key(PositionKey { stream: 0, key: 2, markers: vec![] });
        assert!(table.markers().is_empty());
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

    const NOW: u64 = 1_000;

    #[test]
    fn the_pack_is_offered_once_per_player_on_this_server() {
        let mut gate = PackGate::default();
        assert!(gate.offer(1, NOW));
        assert!(!gate.offer(1, NOW + 5));
        assert!(gate.offer(2, NOW));
        gate.leave(1);
        assert!(gate.offer(1, NOW + 9));
    }

    #[test]
    fn declining_or_failing_disconnects_with_the_right_reason() {
        let mut gate = PackGate::default();
        gate.offer(1, NOW);
        assert_eq!(gate.status(1, PackStatus::Declined, NOW), Some(PackFailure::Declined));
        gate.offer(2, NOW);
        gate.status(2, PackStatus::Accepted, NOW);
        assert_eq!(gate.status(2, PackStatus::Failed, NOW), Some(PackFailure::Failed));
    }

    #[test]
    fn a_finished_pack_ignores_later_answers_and_deadlines() {
        let mut gate = PackGate::default();
        gate.offer(1, NOW);
        for status in [PackStatus::Accepted, PackStatus::Downloaded, PackStatus::Loaded] {
            assert_eq!(gate.status(1, status, NOW), None);
        }
        assert_eq!(gate.status(1, PackStatus::Declined, NOW), None);
        assert!(gate.poll(NOW + 10_000_000).is_empty());
    }

    #[test]
    fn answers_from_strangers_change_nothing() {
        assert_eq!(PackGate::default().status(9, PackStatus::Declined, NOW), None);
    }

    #[test]
    fn silence_times_out_once_and_acceptance_extends_the_deadline() {
        let mut gate = PackGate { answer_ms: 100, apply_ms: 1000, ..PackGate::default() };
        gate.offer(1, 0);
        gate.offer(2, 0);
        gate.status(2, PackStatus::Accepted, 90);
        assert_eq!(gate.poll(100), vec![1]);
        assert!(gate.poll(1000).is_empty());
        assert_eq!(gate.poll(1090), vec![2]);
        assert!(gate.poll(5000).is_empty());
    }

    #[test]
    fn pack_actions_follow_vanillas_numbers() {
        assert_eq!(PackStatus::from_action(0), Some(PackStatus::Loaded));
        assert_eq!(PackStatus::from_action(1), Some(PackStatus::Declined));
        for failed in [2, 5, 6] {
            assert_eq!(PackStatus::from_action(failed), Some(PackStatus::Failed));
        }
        assert_eq!(PackStatus::from_action(3), Some(PackStatus::Accepted));
        assert_eq!(PackStatus::from_action(4), Some(PackStatus::Downloaded));
        assert_eq!(PackStatus::from_action(7), Some(PackStatus::Discarded));
        assert_eq!(PackStatus::from_action(8), None);
    }

    #[test]
    fn the_tick_meter_reports_each_window_and_starts_over() {
        let mut meter = TickMeter::new(100);
        for value in 1..100 {
            assert_eq!(meter.record(value * 1000), None);
        }
        let report = meter.record(1_000_000).unwrap();
        assert_eq!(report, TickReport { ticks: 100, p50_us: 51, p99_us: 99, max_us: 1000 });
        assert_eq!(meter.record(5), None);
    }
}
