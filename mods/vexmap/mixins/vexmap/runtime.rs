// VexMap's glue between the protocol (`vexmap_proto`) and Horizon. Thin on purpose: logic lives in proto.rs, which is
// tested without the game. One bundle serves both sides: a dedicated server is `horizon-client --dedicated`, and each
// side only hears its own hooks.
//
// Phase 1 carries only the handshake and admission (ADR-007). Map features arrive behind feature bits in later phases.

use super::vexmap_link as link;
use super::vexmap_proto as proto;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub(super) const MOD_VERSION: &str = "0.1.0";
/// Modules this client build implements, as bits of `Hello.features`. None yet: each arrives with its phase.
pub(super) const FEATURES: u64 = 0;

/// Modules switched on for this region, kept current by the sidecar link. All off until the sidecar first answers.
static SERVER_FEATURES: AtomicU64 = AtomicU64::new(0);

const KICK_NO_MOD: &str = "VexMap: this server needs the VexMap client mod.\n\
Этому серверу нужен клиентский мод VexMap.";

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn log(message: std::fmt::Arguments<'_>) {
    eprintln!("[vexmap] {message}");
}

// ---------------------------------------------------------------- server

struct Server {
    gate: proto::AdmissionGate,
    inbound: HashMap<u128, proto::Reassembler>,
    outbox: proto::Outbox,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);

fn with_server<R>(f: impl FnOnce(&mut Server) -> R) -> Option<R> {
    let mut guard = SERVER.lock().ok()?;
    let server = guard.get_or_insert_with(|| {
        start_sidecar_link();
        Server {
            gate: proto::AdmissionGate::new(SERVER_FEATURES.load(Ordering::Relaxed)),
            inbound: HashMap::new(),
            outbox: proto::Outbox::default(),
        }
    });
    Some(f(server))
}

/// Runs once, on the first server hook, so a player's client never starts the thread. A failure leaves all modules off
/// and admission working.
fn start_sidecar_link() {
    let addr = std::env::var("VEXMAP_API_ADDR").ok();
    let region = std::env::var("VEXMAP_REGION").ok();
    match link::Config::parse(addr.as_deref(), region.as_deref()) {
        Ok(config) => {
            if let Err(error) = link::start(config, &SERVER_FEATURES) {
                log(format_args!("sidecar link did not start: {error}"));
            }
        }
        Err(error) => log(format_args!("sidecar link is off: {error}")),
    }
}

fn send_to_player(
    game: &mut crate::game::Game<'_>,
    outbox: &mut proto::Outbox,
    player: u128,
    message: &proto::Message,
) {
    let Ok(frames) = outbox.frames(message, proto::MAX_S2C_FRAME) else { return };
    for frame in frames {
        // `Ok` is not delivery (spike S3): anything that must arrive is repeated by the protocol.
        let _ = game.native_send_custom_payload_to_player(player, proto::CHANNEL, &frame);
    }
}

fn kick_text(reason: proto::KickReason) -> String {
    match reason {
        proto::KickReason::NoHello => KICK_NO_MOD.to_owned(),
        proto::KickReason::Protocol { client } => format!(
            "VexMap: update the mod (server protocol {}, yours {client}).\n\
             Обновите мод VexMap (протокол сервера {}, ваш {client}).",
            proto::PROTOCOL,
            proto::PROTOCOL
        ),
    }
}

fn kick(game: &mut crate::game::Game<'_>, outbox: &mut proto::Outbox, player: u128, reason: proto::KickReason) {
    let text = kick_text(reason);
    send_to_player(game, outbox, player, &proto::Message::Reject { reason: text.clone() });
    let queued = game.native_kick(player, &horizon_client::text::Text::literal(text));
    log(format_args!("kick {player:032x} {reason:?} queued={queued}"));
}

/// `player_logged_in`: start the admission clock and ask the client to introduce itself.
pub(super) fn player_logged_in(game: &mut crate::game::Game<'_>, player: u128) {
    with_server(|server| {
        server.gate.join(player, now_ms());
        send_to_player(game, &mut server.outbox, player, &proto::Message::Probe);
    });
}

/// `player_logged_out`: forget everything about the player.
pub(super) fn player_logged_out(player: u128) {
    with_server(|server| {
        server.gate.leave(player);
        server.inbound.remove(&player);
    });
}

/// Once per server tick, on the tick thread. O(players waiting for admission).
pub(super) fn server_tick(game: &mut crate::game::Game<'_>) {
    let now = now_ms();
    with_server(|server| {
        for action in server.gate.poll(now) {
            match action {
                proto::Action::Probe(player) => {
                    send_to_player(game, &mut server.outbox, player, &proto::Message::Probe)
                }
                proto::Action::Kick(player, reason) => kick(game, &mut server.outbox, player, reason),
            }
        }
    });
}

/// A plugin channel message from a client, on the tick thread. Untrusted input.
pub(super) fn payload_from_client(game: &mut crate::game::Game<'_>, player: u128, channel: &str, data: &[u8]) {
    if channel != proto::CHANNEL {
        return;
    }
    let now = now_ms();
    with_server(|server| {
        let reassembler =
            server.inbound.entry(player).or_insert_with(|| proto::Reassembler::new(proto::Limits::default()));
        let proto::Feed::Complete { kind, body } = reassembler.feed(data, now) else { return };
        let Ok(message) = proto::Message::decode(kind, &body) else { return };
        match message {
            proto::Message::Hello { protocol, features, mod_version } => {
                server.gate.server_features = SERVER_FEATURES.load(Ordering::Relaxed);
                match server.gate.hello(player, protocol, features) {
                    proto::HelloOutcome::Welcome { features } => {
                        let region_modules = server.gate.server_features;
                        log(format_args!(
                            "admit {player:032x} mod {mod_version} features {features:#x} region {region_modules:#x}"
                        ));
                        let welcome = proto::Message::Welcome { protocol: proto::PROTOCOL, features };
                        send_to_player(game, &mut server.outbox, player, &welcome);
                    }
                    proto::HelloOutcome::Reject(reason) => kick(game, &mut server.outbox, player, reason),
                }
            }
            proto::Message::Ping(stamp) => {
                send_to_player(game, &mut server.outbox, player, &proto::Message::Pong(stamp))
            }
            _ => {}
        }
    });
}

// ---------------------------------------------------------------- client

struct Client {
    handshake: proto::ClientHandshake,
    inbound: proto::Reassembler,
    outbox: proto::Outbox,
}

static CLIENT: Mutex<Option<Client>> = Mutex::new(None);

fn with_client<R>(f: impl FnOnce(&mut Client) -> R) -> Option<R> {
    let mut guard = CLIENT.lock().ok()?;
    let client = guard.get_or_insert_with(|| Client {
        handshake: proto::ClientHandshake::default(),
        inbound: proto::Reassembler::new(proto::Limits::default()),
        outbox: proto::Outbox::default(),
    });
    Some(f(client))
}

fn payload_packet(channel: &str, data: Vec<u8>) -> horizon_client::mod_api::ServerboundPlayPacket {
    horizon_client::mod_api::ServerboundPlayPacket::CustomPayload(
        horizon_protocol::packets::configuration::CustomPayload { channel: channel.into(), data },
    )
}

fn send_to_server(game: &mut crate::game::Game<'_>, outbox: &mut proto::Outbox, message: &proto::Message) -> bool {
    let Ok(frames) = outbox.frames(message, proto::MAX_C2S_FRAME) else { return false };
    frames.into_iter().all(|frame| game.native_send_packet(payload_packet(proto::CHANNEL, frame)).is_ok())
}

/// A proxy forwards a plugin channel only once the client registered it, so `register` goes before every `Hello`.
fn introduce(game: &mut crate::game::Game<'_>, client: &mut Client) {
    let registered = game.native_send_packet(payload_packet("minecraft:register", proto::CHANNEL.as_bytes().to_vec()));
    if registered.is_err() {
        return;
    }
    let hello =
        proto::Message::Hello { protocol: proto::PROTOCOL, features: FEATURES, mod_version: MOD_VERSION.into() };
    send_to_server(game, &mut client.outbox, &hello);
}

/// Every frame on the window's game. Cheap unless a `Hello` is due (every 3 s until welcomed).
pub(super) fn frame(game: &mut crate::game::Game<'_>) {
    let now = now_ms();
    with_client(|client| {
        if client.handshake.due(now) {
            introduce(game, client);
        }
    });
}

/// Every play packet from the server, on the window's game.
pub(super) fn client_packet(game: &mut crate::game::Game<'_>, packet: &horizon_client::mod_api::ClientboundPlayPacket) {
    let horizon_client::mod_api::ClientboundPlayPacket::CustomPayload(payload) = packet else { return };
    if payload.channel.as_str() != proto::CHANNEL {
        return;
    }
    let now = now_ms();
    with_client(|client| {
        let proto::Feed::Complete { kind, body } = client.inbound.feed(&payload.data, now) else { return };
        let Ok(message) = proto::Message::decode(kind, &body) else { return };
        match message {
            proto::Message::Probe => {
                client.handshake.probed();
                client.handshake.due(now);
                introduce(game, client);
            }
            proto::Message::Welcome { protocol, features } => {
                client.handshake.welcomed();
                log(format_args!("welcomed: protocol {protocol}, features {features:#x}"));
            }
            proto::Message::Reject { reason } => log(format_args!("rejected: {reason}")),
            proto::Message::Ping(stamp) => {
                send_to_server(game, &mut client.outbox, &proto::Message::Pong(stamp));
            }
            _ => {}
        }
    });
}
