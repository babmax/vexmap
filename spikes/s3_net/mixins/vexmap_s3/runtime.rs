// Throwaway spike S3/S4: what does `horizon:mod_net` cost the server tick, and how much reaches a client?
// The server sends VEXMAP_S3_MSGS messages of VEXMAP_S3_SIZE bytes a tick to every player. Incompressible payload: the worst
// case for the hub's compression. The client counts what arrives and echoes a timestamp for the round trip.
// Not for release. All measurements go to stderr as one `[vexmap_s3]` line per interval.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub const DATA: &str = "vexmap_s3:data";
pub const ECHO: &str = "vexmap_s3:echo";
pub const PLUGIN_DATA: &str = "vexmap:data";
pub const PLUGIN_ECHO: &str = "vexmap:echo";
const HEADER: usize = 16;
const REPORT_TICKS: u32 = 200;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn template() -> &'static (Vec<u8>, usize) {
    static TEMPLATE: OnceLock<(Vec<u8>, usize)> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let size = env_usize("VEXMAP_S3_SIZE", 0).clamp(HEADER, 30_000);
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let bytes = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        (bytes, env_usize("VEXMAP_S3_MSGS", 0))
    })
}

fn plugin_transport() -> bool {
    std::env::var("VEXMAP_S3_TRANSPORT").is_ok_and(|v| v == "plugin")
}

#[derive(Default)]
struct ServerStats {
    ticks: u32,
    hook_nanos: Vec<u64>,
    rtt_micros: Vec<u64>,
    sent_bytes: u64,
    send_errors: u64,
    seq: u64,
}

static SERVER: Mutex<Option<ServerStats>> = Mutex::new(None);

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

/// Once per server tick, on the tick thread.
pub fn server_tick(game: &mut crate::game::Game<'_>) {
    let started = Instant::now();
    let (template, msgs) = template();
    let players = game.native_server_players();
    let Ok(mut guard) = SERVER.lock() else { return };
    let stats = guard.get_or_insert_with(ServerStats::default);
    let now = epoch().elapsed().as_micros() as u64;
    let mut buffer = template.clone();
    for player in &players {
        for index in 0..*msgs {
            stats.seq += 1;
            buffer[..8].copy_from_slice(&stats.seq.to_le_bytes());
            let stamp = if index == 0 { now } else { 0 };
            buffer[8..16].copy_from_slice(&stamp.to_le_bytes());
            let sent = if plugin_transport() {
                game.native_send_custom_payload_to_player(player.uuid, PLUGIN_DATA, &buffer)
            } else {
                game.native_net_send_to_player(player.uuid, DATA, &buffer)
            };
            match sent {
                Ok(()) => stats.sent_bytes += buffer.len() as u64,
                Err(_) => stats.send_errors += 1,
            }
        }
    }
    stats.hook_nanos.push(started.elapsed().as_nanos() as u64);
    stats.ticks += 1;
    if stats.ticks < REPORT_TICKS {
        return;
    }
    let (rate, mspt) = game.native_tick_stats();
    stats.hook_nanos.sort_unstable();
    stats.rtt_micros.sort_unstable();
    eprintln!(
        "[vexmap_s3] server players={} size={} msgs={} tps={:.2} mspt_avg={:.2} hook_us_p50={:.0} hook_us_p99={:.0} hook_us_max={:.0} sent_kb_s_per_player={:.1} send_errors={} rtt_ms_p50={:.1} rtt_ms_p99={:.1} rtt_samples={}",
        players.len(),
        template.len(),
        msgs,
        rate,
        mspt,
        percentile(&stats.hook_nanos, 0.5) as f64 / 1000.0,
        percentile(&stats.hook_nanos, 0.99) as f64 / 1000.0,
        percentile(&stats.hook_nanos, 1.0) as f64 / 1000.0,
        stats.sent_bytes as f64 / 1024.0 / (f64::from(REPORT_TICKS) / 20.0) / players.len().max(1) as f64,
        stats.send_errors,
        percentile(&stats.rtt_micros, 0.5) as f64 / 1000.0,
        percentile(&stats.rtt_micros, 0.99) as f64 / 1000.0,
        stats.rtt_micros.len(),
    );
    *stats = ServerStats { seq: stats.seq, ..ServerStats::default() };
}

/// A client's echo, on the tick thread. What a client sends is its own input: read it with care.
pub fn from_client(channel: &str, data: &[u8]) {
    if (channel != ECHO && channel != PLUGIN_ECHO) || data.len() != 8 {
        return;
    }
    let mut stamp = [0; 8];
    stamp.copy_from_slice(data);
    let sent = u64::from_le_bytes(stamp);
    let now = epoch().elapsed().as_micros() as u64;
    if let (Ok(mut guard), true) = (SERVER.lock(), sent <= now) {
        guard.get_or_insert_with(ServerStats::default).rtt_micros.push(now - sent);
    }
}

static CLIENT_BYTES: AtomicU64 = AtomicU64::new(0);
static CLIENT_MESSAGES: AtomicU64 = AtomicU64::new(0);
static CLIENT_LAST_SEQ: AtomicU64 = AtomicU64::new(0);
static CLIENT_OUT_OF_ORDER: AtomicU64 = AtomicU64::new(0);
static CLIENT_REPORT: Mutex<Option<Instant>> = Mutex::new(None);

/// A message from the server, on the window's game.
pub fn from_server(game: &mut crate::game::Game<'_>, channel: &str, data: &[u8]) {
    if (channel != DATA && channel != PLUGIN_DATA) || data.len() < HEADER {
        return;
    }
    CLIENT_BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
    CLIENT_MESSAGES.fetch_add(1, Ordering::Relaxed);
    let mut seq = [0; 8];
    seq.copy_from_slice(&data[..8]);
    let seq = u64::from_le_bytes(seq);
    if seq <= CLIENT_LAST_SEQ.swap(seq, Ordering::Relaxed) {
        CLIENT_OUT_OF_ORDER.fetch_add(1, Ordering::Relaxed);
    }
    if data[8..16] != [0; 8] {
        if channel == PLUGIN_DATA {
            let _ = game.native_send_packet(payload_packet(PLUGIN_ECHO, data[8..16].to_vec()));
        } else {
            let _ = game.native_net_send_to_server(ECHO, &data[8..16]);
        }
    }
    let Ok(mut report) = CLIENT_REPORT.lock() else { return };
    let last = report.get_or_insert_with(Instant::now);
    let seconds = last.elapsed().as_secs_f64();
    if seconds >= 10.0 {
        eprintln!(
            "[vexmap_s3] client received_kb_s={:.1} messages_s={:.1} out_of_order={}",
            CLIENT_BYTES.swap(0, Ordering::Relaxed) as f64 / 1024.0 / seconds,
            CLIENT_MESSAGES.swap(0, Ordering::Relaxed) as f64 / seconds,
            CLIENT_OUT_OF_ORDER.swap(0, Ordering::Relaxed),
        );
        *last = Instant::now();
    }
}

fn payload_packet(channel: &str, data: Vec<u8>) -> horizon_client::mod_api::ServerboundPlayPacket {
    horizon_client::mod_api::ServerboundPlayPacket::CustomPayload(horizon_protocol::packets::configuration::CustomPayload {
        channel: channel.into(),
        data,
    })
}

/// A plugin message from the server, heard on the window's game.
pub fn client_packet(game: &mut crate::game::Game<'_>, packet: &horizon_client::mod_api::ClientboundPlayPacket) {
    if let horizon_client::mod_api::ClientboundPlayPacket::CustomPayload(payload) = packet {
        if payload.channel.as_str() == PLUGIN_DATA {
            from_server(game, PLUGIN_DATA, &payload.data);
        }
    }
}

static REGISTERED: Mutex<Option<Instant>> = Mutex::new(None);

/// A proxy forwards a plugin channel to a client only once that client has registered it: `minecraft:register`.
/// Repeated every few seconds because this spike has no join event for a proxy's server switch.
pub fn frame(game: &mut crate::game::Game<'_>) {
    let Ok(mut last) = REGISTERED.lock() else { return };
    if last.is_some_and(|at| at.elapsed().as_secs() < 3) {
        return;
    }
    if game.native_send_packet(payload_packet("minecraft:register", format!("{PLUGIN_DATA} {PLUGIN_ECHO}").into_bytes())).is_ok() {
        *last = Some(Instant::now());
    }
}
