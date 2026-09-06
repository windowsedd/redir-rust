//! Bedrock ("MCPE") offline-server support for the UDP path: when the real
//! backend target is unreachable, this plugin answers on its behalf so
//! players see a proper message instead of a timeout.
//!
//! Two independent pieces, covering the two ways a Bedrock client probes a
//! server:
//! - **Server list ping**: `Unconnected Ping` gets a synthesized
//!   `Unconnected Pong` carrying a canned MOTD, with no RakNet connection
//!   involved at all.
//! - **Join attempt**: a `FakeSession` completes just enough of the RakNet
//!   handshake and the pre-login Minecraft handshake
//!   (`RequestNetworkSettings`/`NetworkSettings`) to reach a point where the
//!   client accepts a `Disconnect` game packet carrying the same MOTD text,
//!   then tears the fake session down.
//!
//! The join-attempt path deliberately does not implement RakNet reliability
//! in full (no retransmission, no NACKs): the handshake is short enough
//! that if a datagram is lost, the real client simply retries the step it's
//! stuck on and we handle it idempotently. Fragmented frames (`Login`'s JWT
//! chain/skin data routinely exceeds one datagram's MTU) aren't reassembled
//! since we don't need their contents -- see `parse_fragment_header`.
//!
//! Wire format notes (observed against a live client, mid-2026, and cross-
//! checked against pmmp/BedrockProtocol, an actively maintained reference):
//! - Every game packet, including `RequestNetworkSettings`, arrives wrapped
//!   in the `0xFE` batch header (varint sub-packet length + packet bytes,
//!   possibly several per batch) -- there is no raw/unwrapped special case.
//! - We negotiate compression algorithm `0xffff` ("none") so the rest of
//!   the exchange (just our `Disconnect`) needs no compression library.
//! - `Disconnect`'s payload is `reason: signed VarInt` + `type: unsigned
//!   VarInt` (0 = message follows) +, if not skipped, a `message` string
//!   and a `filteredMessage` string (always a pair). An earlier attempt
//!   that treated this as a plain `bool hide_reason` + one string shifted
//!   every field by a byte and silently corrupted the packet client-side.
//! - RakNet keep-alive traffic (`ConnectedPing`, id 0x00) can arrive
//!   interleaved at any point once the RakNet handshake is done; it's not a
//!   game packet and must be ignored rather than mistaken for a login.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::debug;

/// Fixed 16-byte magic every RakNet offline-message packet must contain;
/// used both to recognize inbound pings/handshake packets and to stamp
/// outbound replies.
const RAKNET_MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];

const ID_UNCONNECTED_PING: u8 = 0x01;
const ID_UNCONNECTED_PING_OPEN_CONNECTIONS: u8 = 0x02;
const ID_UNCONNECTED_PONG: u8 = 0x1c;

/// Smallest valid Unconnected Ping: 1 (id) + 8 (time) + 16 (magic) = 25.
/// A real client also appends an 8-byte client GUID, but we don't need it.
const MIN_PING_LEN: usize = 1 + 8 + 16;

const ID_OPEN_CONNECTION_REQUEST_1: u8 = 0x05;
const ID_OPEN_CONNECTION_REPLY_1: u8 = 0x06;
const ID_OPEN_CONNECTION_REQUEST_2: u8 = 0x07;
const ID_OPEN_CONNECTION_REPLY_2: u8 = 0x08;
const ID_CONNECTION_REQUEST: u8 = 0x09;
const ID_CONNECTION_REQUEST_ACCEPTED: u8 = 0x10;
const ID_NEW_INCOMING_CONNECTION: u8 = 0x13;
const ID_CONNECTED_PING: u8 = 0x00;

const RELIABLE_ORDERED: u8 = 3;

const MC_ID_REQUEST_NETWORK_SETTINGS: [u8; 2] = [0xC1, 0x01];
const MC_ID_NETWORK_SETTINGS: [u8; 2] = [0x8F, 0x01];
const MC_ID_DISCONNECT: u8 = 0x05;
const MC_BATCH_HEADER: u8 = 0xFE;

/// How long an in-progress fake handshake can sit idle before we drop its
/// state; a real client that's still trying will simply start over.
pub const FAKE_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Real Bedrock login payloads use far fewer fragments. This generous ceiling
/// keeps realistic large logins working while bounding attacker-owned state.
const MAX_TRACKED_LOGIN_FRAGMENTS: u32 = 4_096;

#[derive(Debug, Clone)]
pub struct BedrockOfflineConfig {
    pub motd_line1: String,
    pub motd_line2: String,
    /// Stable per-process id reported as this "server"'s GUID; doesn't need
    /// to match anything real since we never accept an actual connection.
    pub server_guid: i64,
}

impl BedrockOfflineConfig {
    pub fn new(motd_line1: String, motd_line2: String) -> Self {
        let server_guid = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Self {
            motd_line1,
            motd_line2,
            server_guid,
        }
    }
}

// No leading `§` color codes here (unlike the TCP/Java plugin's defaults):
// they render fine in the server-list MOTD ping, but Bedrock's disconnect
// screen is a different UI layer that shows them literally instead of
// applying color, so plain text keeps both paths looking clean.
pub fn default_motd_line1() -> String {
    "伺服器無法連線！ / Server unreachable!".to_string()
}

pub fn default_motd_line2() -> String {
    "請稍後再試 / Please Try Again".to_string()
}

// ---------------------------------------------------------------------
// Server list ping (Unconnected Ping/Pong)
// ---------------------------------------------------------------------

/// Whether `data` looks like a RakNet Unconnected Ping (either variant),
/// i.e. carries the correct id and the fixed offline-message magic.
pub fn is_unconnected_ping(data: &[u8]) -> bool {
    if data.len() < MIN_PING_LEN {
        return false;
    }
    let id = data[0];
    if id != ID_UNCONNECTED_PING && id != ID_UNCONNECTED_PING_OPEN_CONNECTIONS {
        return false;
    }
    data[9..25] == RAKNET_MAGIC
}

/// Builds an Unconnected Pong reply to `ping`, echoing its timestamp and
/// carrying the configured MOTD. Returns `None` if `ping` isn't a
/// recognizable Unconnected Ping.
pub fn build_unconnected_pong(
    ping: &[u8],
    config: &BedrockOfflineConfig,
    listen_port: u16,
) -> Option<Vec<u8>> {
    if !is_unconnected_ping(ping) {
        return None;
    }
    let time: [u8; 8] = ping[1..9].try_into().ok()?;

    let motd = format!(
        "MCPE;{};766;1.21.60;0;20;{};{};Survival;1;{};{};",
        sanitize(&config.motd_line1),
        config.server_guid,
        sanitize(&config.motd_line2),
        listen_port,
        listen_port,
    );
    let motd_bytes = motd.as_bytes();

    let mut out = Vec::with_capacity(1 + 8 + 8 + 16 + 2 + motd_bytes.len());
    out.push(ID_UNCONNECTED_PONG);
    out.extend_from_slice(&time);
    out.extend_from_slice(&config.server_guid.to_be_bytes());
    out.extend_from_slice(&RAKNET_MAGIC);
    out.extend_from_slice(&(motd_bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(motd_bytes);
    Some(out)
}

/// RakNet MOTD fields are semicolon-delimited; strip any that sneak into
/// configured text so a malformed value can't corrupt the packet framing.
fn sanitize(field: &str) -> String {
    field.replace(';', ",")
}

fn build_unconnected_ping_probe() -> Vec<u8> {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut out = Vec::with_capacity(MIN_PING_LEN);
    out.push(ID_UNCONNECTED_PING);
    out.extend_from_slice(&time.to_be_bytes());
    out.extend_from_slice(&RAKNET_MAGIC);
    out
}

/// One-shot reachability probe: opens a scratch socket, sends an
/// Unconnected Ping to `target_addr`, and waits up to `probe_timeout` for a
/// valid Unconnected Pong.
pub async fn probe_target(target_addr: SocketAddr, probe_timeout: Duration) -> bool {
    let bind_addr = if target_addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = match UdpSocket::bind(bind_addr).await {
        Ok(s) => s,
        Err(_) => return false,
    };
    if socket.connect(target_addr).await.is_err() {
        return false;
    }
    let ping = build_unconnected_ping_probe();
    if socket.send(&ping).await.is_err() {
        return false;
    }
    let mut buf = vec![0u8; 35 + usize::from(u16::MAX)];
    let Ok(Ok(n)) = timeout(probe_timeout, socket.recv(&mut buf)).await else {
        return false;
    };
    if n < 35 {
        return false;
    }
    let declared_length = usize::from(u16::from_be_bytes([buf[33], buf[34]]));
    buf[0] == ID_UNCONNECTED_PONG
        && buf[1..9] == ping[1..9]
        && buf[17..33] == RAKNET_MAGIC
        && 35usize.checked_add(declared_length) == Some(n)
}

// ---------------------------------------------------------------------
// Join attempt (fake RakNet + pre-login handshake, ending in Disconnect)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    AwaitingConnectionRequest,
    AwaitingNewIncomingConnection,
    AwaitingNetworkSettingsRequest,
    AwaitingLoginAttempt,
    Done,
}

pub struct FakeSession {
    stage: Stage,
    out_seq: u32,
    out_reliable_index: u32,
    out_order_index: u32,
    pub last_activity: Instant,
    /// Tracks a Login split-packet in progress: (split_id, split_count,
    /// fragment indices seen so far). `Login` carries several KB of JWT
    /// chain/skin data, so it always arrives as multiple RakNet fragments;
    /// disconnecting after only the first one would kick the client mid-send
    /// of its own packet, which plausibly reads to it as a protocol
    /// violation rather than a normal kick.
    pending_split: Option<(u16, u32, HashSet<u32>)>,
}

impl FakeSession {
    fn new() -> Self {
        Self {
            stage: Stage::AwaitingConnectionRequest,
            out_seq: 0,
            out_reliable_index: 0,
            out_order_index: 0,
            last_activity: Instant::now(),
            pending_split: None,
        }
    }

    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    /// Records one arrived fragment of a split packet, resetting tracking
    /// if `split_id` changes (a new split superseding whatever was pending).
    /// Returns `(fragments_seen, fragments_expected)`, or `None` for invalid
    /// or inconsistent fragment metadata.
    fn record_fragment(
        &mut self,
        split_id: u16,
        split_count: u32,
        split_index: u32,
    ) -> Option<(usize, u32)> {
        if split_count == 0 || split_count > MAX_TRACKED_LOGIN_FRAGMENTS {
            self.pending_split = None;
            return None;
        }
        if let Some((id, count, _)) = &self.pending_split {
            if *id == split_id && *count != split_count {
                self.pending_split = None;
                return None;
            }
        }
        if split_index >= split_count {
            return None;
        }

        if !matches!(&self.pending_split, Some((id, _, _)) if *id == split_id) {
            self.pending_split = Some((split_id, split_count, HashSet::new()));
        }
        let (_, count, seen) = self.pending_split.as_mut().unwrap();
        seen.insert(split_index);
        Some((seen.len(), *count))
    }

    fn wrap(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(10 + payload.len());
        frame.push(RELIABLE_ORDERED << 5);
        frame.extend_from_slice(&((payload.len() as u16) * 8).to_be_bytes());
        frame.extend_from_slice(&self.out_reliable_index.to_le_bytes()[..3]);
        frame.extend_from_slice(&self.out_order_index.to_le_bytes()[..3]);
        frame.push(0); // order channel

        frame.extend_from_slice(payload);
        self.out_reliable_index += 1;
        self.out_order_index += 1;

        let mut datagram = Vec::with_capacity(4 + frame.len());
        datagram.push(0x84); // valid datagram
        datagram.extend_from_slice(&self.out_seq.to_le_bytes()[..3]);
        self.out_seq += 1;
        datagram.extend_from_slice(&frame);
        datagram
    }
}

pub fn new_session() -> FakeSession {
    FakeSession::new()
}

/// Handles one client's Bedrock offline-kick handshake. Called once per
/// inbound datagram from a client whose session is (or should become) a
/// fake connection; owns a `FakeSession` entry in the caller's map.
///
/// Returns the datagrams to send back to the client, in order.
pub fn handle_datagram(
    session: &mut FakeSession,
    data: &[u8],
    client_addr: SocketAddr,
    server_guid: i64,
    motd_line1: &str,
    motd_line2: &str,
) -> Vec<Vec<u8>> {
    let previous_activity = session.last_activity;
    session.last_activity = Instant::now();
    let mut out = Vec::new();

    if data.is_empty() {
        return out;
    }
    debug!(client = %client_addr, len = data.len(), first_byte = format!("{:#04x}", data[0]), "bedrock offline: inbound datagram");

    // Unencapsulated handshake packets (before any reliability layer).
    if data[0] == ID_OPEN_CONNECTION_REQUEST_1 {
        if let Some(reply) = build_open_connection_reply_1(data, server_guid) {
            out.push(reply);
        }
        return out;
    }
    if data[0] == ID_OPEN_CONNECTION_REQUEST_2 {
        if let Some(reply) = build_open_connection_reply_2(data, client_addr, server_guid) {
            out.push(reply);
        }
        return out;
    }

    // Everything past this point is expected to be a Datagram frame set
    // (top bit set, not an ACK/NACK).
    if data[0] & 0x80 == 0 || data[0] == 0xA0 || data[0] == 0xC0 {
        return out;
    }

    // RakNet clients expect every reliable datagram they send to be
    // acknowledged; without this, the client's own reliability layer times
    // out the connection after a couple of unacked sends and bails with its
    // own generic "disconnected" error before we ever see its next packet.
    if data.len() >= 4 {
        let seq = u32::from_le_bytes([data[1], data[2], data[3], 0]);
        out.push(build_ack(seq));
    }

    let frames = parse_frame_payloads(data);
    if frames.is_empty() {
        debug!(client = %client_addr, raw = %hex(data), "bedrock offline: datagram produced no parseable frames");
    }

    for payload in &frames {
        match session.stage {
            Stage::AwaitingConnectionRequest if payload.first() == Some(&ID_CONNECTION_REQUEST) => {
                if payload.len() >= 17 {
                    let request_timestamp = &payload[9..17];
                    let reply =
                        build_connection_request_accepted(session, client_addr, request_timestamp);
                    out.push(reply);
                    session.stage = Stage::AwaitingNewIncomingConnection;
                    debug!(client = %client_addr, "bedrock offline: connection request accepted");
                }
            }
            Stage::AwaitingNewIncomingConnection
                if payload.first() == Some(&ID_NEW_INCOMING_CONNECTION) =>
            {
                session.stage = Stage::AwaitingNetworkSettingsRequest;
                debug!(client = %client_addr, "bedrock offline: new incoming connection");
            }
            Stage::AwaitingNetworkSettingsRequest
                if split_batch_packets(payload)
                    .iter()
                    .any(|pkt| pkt.starts_with(&MC_ID_REQUEST_NETWORK_SETTINGS)) =>
            {
                let reply = build_network_settings(session);
                out.push(reply);
                session.stage = Stage::AwaitingLoginAttempt;
                debug!(client = %client_addr, "bedrock offline: sent network settings (compression=none)");
            }
            Stage::AwaitingLoginAttempt if payload.first() == Some(&ID_CONNECTED_PING) => {
                // RakNet keep-alive, not a game packet; ignore and keep waiting.
            }
            Stage::AwaitingLoginAttempt => {
                let reply = build_disconnect(session, motd_line1, motd_line2);
                out.push(reply);
                session.stage = Stage::Done;
                debug!(client = %client_addr, "bedrock offline: sent disconnect with offline motd");
                break;
            }
            _ => {
                debug!(
                    client = %client_addr,
                    stage = ?session.stage,
                    raw = %hex(payload),
                    "bedrock offline: unrecognized frame for current stage"
                );
            }
        }
    }

    // Login carries JWT chain/skin data that's typically several KB --
    // well past one datagram's MTU, so it arrives RakNet-fragmented. We
    // don't reassemble fragments' contents (see module docs) since we don't
    // need them, but we do need to wait for every fragment to arrive before
    // disconnecting: kicking the client after only the first piece of its
    // own in-flight packet is a protocol-order violation from its side, and
    // plausibly why it was showing its own generic error instead of a clean
    // kick screen.
    if frames.is_empty() && session.stage == Stage::AwaitingLoginAttempt {
        let ready = match parse_fragment_header(data) {
            Some(frag) => {
                match session.record_fragment(frag.split_id, frag.split_count, frag.split_index) {
                    Some((seen, expected)) => {
                        debug!(client = %client_addr, seen, expected, "bedrock offline: login fragment received");
                        seen as u32 >= expected
                    }
                    None => {
                        session.last_activity = previous_activity;
                        false
                    }
                }
            }
            None if data.get(4).is_some_and(|flags| flags & 0x10 != 0) => {
                session.last_activity = previous_activity;
                false
            }
            // This is not a fragmented datagram, so retain the existing
            // fallback for small unfragmented login/test packets.
            None => true,
        };

        if ready {
            let reply = build_disconnect(session, motd_line1, motd_line2);
            out.push(reply);
            session.stage = Stage::Done;
            debug!(client = %client_addr, "bedrock offline: sent disconnect with offline motd");
        }
    }

    out
}

/// Single-record ACK for the given datagram sequence number. We never
/// re-send anything ourselves, so we don't need NACK handling or ACK
/// range-coalescing -- one record per received datagram is enough to keep
/// the client's reliability layer satisfied.
fn build_ack(seq: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.push(0xC0); // ACK
    out.extend_from_slice(&1u16.to_be_bytes()); // record count
    out.push(1); // record is a single index, not a range
    out.extend_from_slice(&seq.to_le_bytes()[..3]);
    out
}

fn build_open_connection_reply_1(request: &[u8], server_guid: i64) -> Option<Vec<u8>> {
    if request.len() < 17 || request[1..17] != RAKNET_MAGIC {
        return None;
    }
    let mtu = (request.len() as u16).max(46).min(1492);

    let mut out = Vec::with_capacity(28);
    out.push(ID_OPEN_CONNECTION_REPLY_1);
    out.extend_from_slice(&RAKNET_MAGIC);
    out.extend_from_slice(&server_guid.to_be_bytes());
    out.push(0); // use_security = false
    out.extend_from_slice(&mtu.to_be_bytes());
    Some(out)
}

fn build_open_connection_reply_2(
    request: &[u8],
    client_addr: SocketAddr,
    server_guid: i64,
) -> Option<Vec<u8>> {
    if request.len() < 34 || request[1..17] != RAKNET_MAGIC {
        return None;
    }
    let mtu = u16::from_be_bytes([request[24], request[25]]);

    let mut out = Vec::with_capacity(32);
    out.push(ID_OPEN_CONNECTION_REPLY_2);
    out.extend_from_slice(&RAKNET_MAGIC);
    out.extend_from_slice(&server_guid.to_be_bytes());
    out.extend_from_slice(&encode_address(client_addr));
    out.extend_from_slice(&mtu.to_be_bytes());
    out.push(0); // use_encryption = false
    Some(out)
}

fn build_connection_request_accepted(
    session: &mut FakeSession,
    client_addr: SocketAddr,
    request_timestamp: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(96);
    body.push(ID_CONNECTION_REQUEST_ACCEPTED);
    body.extend_from_slice(&encode_address(client_addr));
    body.extend_from_slice(&0u16.to_be_bytes()); // system index

    let padding = encode_address("0.0.0.0:0".parse().unwrap());
    for _ in 0..10 {
        body.extend_from_slice(&padding);
    }

    body.extend_from_slice(request_timestamp);
    body.extend_from_slice(&now_millis().to_be_bytes());

    session.wrap(&body)
}

fn build_network_settings(session: &mut FakeSession) -> Vec<u8> {
    let mut game_packet = Vec::with_capacity(12);
    game_packet.extend_from_slice(&MC_ID_NETWORK_SETTINGS);
    game_packet.extend_from_slice(&0u16.to_le_bytes()); // compression_threshold (unused when algorithm=none)
    game_packet.extend_from_slice(&0xFFFFu16.to_le_bytes()); // compression_algorithm = none
    game_packet.push(0); // client_throttle_enabled = false
    game_packet.push(0); // client_throttle_threshold
    game_packet.extend_from_slice(&0f32.to_le_bytes()); // client_throttle_scalar

    let mut batch = Vec::with_capacity(2 + game_packet.len());
    batch.push(MC_BATCH_HEADER);
    batch.extend_from_slice(&encode_varint(game_packet.len() as u32));
    batch.extend_from_slice(&game_packet);

    session.wrap(&batch)
}

fn build_disconnect(session: &mut FakeSession, motd_line1: &str, motd_line2: &str) -> Vec<u8> {
    let message = format!("{motd_line1}\n{motd_line2}");
    let message_bytes = message.as_bytes();

    // Per pmmp/BedrockProtocol's DisconnectPacket (authoritative, actively
    // maintained), the payload is:
    //   reason: signed VarInt (zigzag) -- we always send 0 ("unknown").
    //   type: unsigned VarInt -- 0 means a message follows, anything else
    //     means no message. NOT a plain bool.
    //   if type == 0: message (string), filteredMessage (string) -- always
    //     a pair, not independently optional.
    let mut game_packet = Vec::with_capacity(6 + message_bytes.len() * 2);
    game_packet.push(MC_ID_DISCONNECT);
    game_packet.extend_from_slice(&encode_varint(zigzag_encode(0))); // reason = 0
    game_packet.extend_from_slice(&encode_varint(0)); // type = 0 (message follows)
    game_packet.extend_from_slice(&encode_varint(message_bytes.len() as u32));
    game_packet.extend_from_slice(message_bytes);
    game_packet.extend_from_slice(&encode_varint(message_bytes.len() as u32)); // filteredMessage
    game_packet.extend_from_slice(message_bytes);

    let mut batch = Vec::with_capacity(2 + game_packet.len());
    batch.push(MC_BATCH_HEADER);
    batch.extend_from_slice(&encode_varint(game_packet.len() as u32));
    batch.extend_from_slice(&game_packet);

    session.wrap(&batch)
}

fn encode_address(addr: SocketAddr) -> [u8; 7] {
    let mut out = [0u8; 7];
    out[0] = 4; // IPv4
    if let SocketAddr::V4(v4) = addr {
        out[1..5].copy_from_slice(&v4.ip().octets());
    }
    out[5..7].copy_from_slice(&addr.port().to_be_bytes());
    out
}

/// Splits a `0xFE`-prefixed batch into its varint-length-prefixed
/// sub-packets. If `payload` isn't a batch at all, returns it unchanged as
/// a single "packet" so callers don't need to special-case non-batched
/// frames (kept for robustness, even though every game packet observed in
/// practice is batched).
fn split_batch_packets(payload: &[u8]) -> Vec<&[u8]> {
    if payload.first() != Some(&MC_BATCH_HEADER) {
        return vec![payload];
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < payload.len() {
        let Some((len, consumed)) = decode_varint(&payload[i..]) else {
            break;
        };
        i += consumed;
        let Some(pkt) = payload.get(i..i + len as usize) else {
            break;
        };
        out.push(pkt);
        i += len as usize;
    }
    out
}

fn decode_varint(data: &[u8]) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate().take(5) {
        value |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
    }
    None
}

/// Zigzag-encodes a signed value for use with `encode_varint` (used for
/// Bedrock's signed VarInt fields, e.g. `Disconnect`'s `reason`).
fn zigzag_encode(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

fn encode_varint(mut value: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn hex(data: &[u8]) -> String {
    const MAX_LOGGED_BYTES: usize = 64;
    let truncated = data.len() > MAX_LOGGED_BYTES;
    let shown = &data[..data.len().min(MAX_LOGGED_BYTES)];
    let mut out = shown
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    if truncated {
        out.push_str(&format!(" ... ({} bytes total)", data.len()));
    }
    out
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Extracts each frame's payload from a RakNet Datagram frame set. Bails
/// (returns what it found so far) on any fragmented frame or malformed
/// length, since we don't support reassembly -- our handshake packets are
/// always small enough not to need it, and a client sending a fragmented
/// frame this early just won't progress, same as a dropped datagram.
fn parse_frame_payloads(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if data.len() < 4 {
        return out;
    }
    let mut i = 4; // skip datagram header (flags + 3-byte sequence number)

    while i < data.len() {
        let Some(&frame_flags) = data.get(i) else {
            break;
        };
        let reliability = (frame_flags >> 5) & 0x07;
        let fragmented = frame_flags & 0x10 != 0;
        i += 1;

        let Some(len_bits_bytes) = data.get(i..i + 2) else {
            break;
        };
        let len_bits = u16::from_be_bytes([len_bits_bytes[0], len_bits_bytes[1]]);
        let len_bytes = (len_bits as usize).div_ceil(8);
        i += 2;

        // Reliable message index (3 bytes) for reliability types that carry one.
        if matches!(reliability, 2 | 3 | 4 | 6 | 7) {
            if data.get(i..i + 3).is_none() {
                break;
            }
            i += 3;
        }
        // Order index (3 bytes) + order channel (1 byte) for ordered/sequenced types.
        if matches!(reliability, 1 | 3 | 4 | 5 | 7) {
            if data.get(i..i + 4).is_none() {
                break;
            }
            i += 4;
        }

        if fragmented {
            break; // unsupported; see doc comment above
        }

        let Some(payload) = data.get(i..i + len_bytes) else {
            break;
        };
        out.push(payload);
        i += len_bytes;
    }

    out
}

struct FragmentHeader {
    split_count: u32,
    split_id: u16,
    split_index: u32,
}

/// Parses just the header fields of a datagram's first (and, for our
/// purposes, only relevant) frame when it's fragmented -- the split
/// bookkeeping fields RakNet adds after the usual reliable/order index
/// fields, without attempting to reassemble the fragment's actual payload
/// (see `parse_frame_payloads`, which bails on fragmented frames entirely).
fn parse_fragment_header(data: &[u8]) -> Option<FragmentHeader> {
    let mut i = 4usize; // skip datagram header (flags + 3-byte sequence number)

    let frame_flags = *data.get(i)?;
    let reliability = (frame_flags >> 5) & 0x07;
    let fragmented = frame_flags & 0x10 != 0;
    i += 1;

    i += 2; // length in bits, unused here
    if matches!(reliability, 2 | 3 | 4 | 6 | 7) {
        i += 3; // reliable message index
    }
    if matches!(reliability, 1 | 3 | 4 | 5 | 7) {
        i += 4; // order index + order channel
    }

    if !fragmented {
        return None;
    }

    let split_count = u32::from_be_bytes(data.get(i..i + 4)?.try_into().ok()?);
    i += 4;
    let split_id = u16::from_be_bytes(data.get(i..i + 2)?.try_into().ok()?);
    i += 2;
    let split_index = u32::from_be_bytes(data.get(i..i + 4)?.try_into().ok()?);

    Some(FragmentHeader {
        split_count,
        split_id,
        split_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ping() -> Vec<u8> {
        let mut ping = vec![ID_UNCONNECTED_PING];
        ping.extend_from_slice(&123i64.to_be_bytes());
        ping.extend_from_slice(&RAKNET_MAGIC);
        ping.extend_from_slice(&999i64.to_be_bytes()); // client guid
        ping
    }

    #[test]
    fn recognizes_valid_ping() {
        assert!(is_unconnected_ping(&sample_ping()));
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut ping = sample_ping();
        ping[9] ^= 0xff;
        assert!(!is_unconnected_ping(&ping));
    }

    #[test]
    fn rejects_short_packet() {
        assert!(!is_unconnected_ping(&[ID_UNCONNECTED_PING, 0, 0]));
    }

    #[test]
    fn builds_pong_echoing_time_and_motd() {
        let ping = sample_ping();
        let config = BedrockOfflineConfig {
            motd_line1: "Hello".to_string(),
            motd_line2: "World".to_string(),
            server_guid: 42,
        };
        let pong = build_unconnected_pong(&ping, &config, 19132).unwrap();

        assert_eq!(pong[0], ID_UNCONNECTED_PONG);
        assert_eq!(&pong[1..9], &123i64.to_be_bytes());
        assert_eq!(&pong[9..17], &42i64.to_be_bytes());
        assert_eq!(&pong[17..33], &RAKNET_MAGIC);

        let motd = String::from_utf8(pong[35..].to_vec()).unwrap();
        assert!(motd.starts_with("MCPE;Hello;"));
        assert!(motd.contains(";World;"));
    }

    #[test]
    fn sanitizes_semicolons_in_motd_fields() {
        let ping = sample_ping();
        let config = BedrockOfflineConfig {
            motd_line1: "a;b".to_string(),
            motd_line2: "c".to_string(),
            server_guid: 1,
        };
        let pong = build_unconnected_pong(&ping, &config, 19132).unwrap();
        let motd = String::from_utf8(pong[35..].to_vec()).unwrap();
        assert!(motd.starts_with("MCPE;a,b;"));
    }

    #[test]
    fn open_connection_reply_1_echoes_magic_and_guid() {
        let mut req = vec![ID_OPEN_CONNECTION_REQUEST_1];
        req.extend_from_slice(&RAKNET_MAGIC);
        req.push(11); // raknet protocol version
        req.extend_from_slice(&[0u8; 20]); // MTU padding

        let reply = build_open_connection_reply_1(&req, 42).unwrap();
        assert_eq!(reply[0], ID_OPEN_CONNECTION_REPLY_1);
        assert_eq!(&reply[1..17], &RAKNET_MAGIC);
        assert_eq!(&reply[17..25], &42i64.to_be_bytes());
    }

    #[test]
    fn open_connection_reply_2_encodes_client_address() {
        let mut req = vec![ID_OPEN_CONNECTION_REQUEST_2];
        req.extend_from_slice(&RAKNET_MAGIC);
        req.extend_from_slice(&[4, 127, 0, 0, 1, 0x4d, 0x62]); // server_address (unused)
        req.extend_from_slice(&1400u16.to_be_bytes()); // mtu
        req.extend_from_slice(&999i64.to_be_bytes()); // client guid

        let client_addr: SocketAddr = "127.0.0.1:55943".parse().unwrap();
        let reply = build_open_connection_reply_2(&req, client_addr, 42).unwrap();

        assert_eq!(reply[0], ID_OPEN_CONNECTION_REPLY_2);
        let addr_bytes = &reply[25..32];
        assert_eq!(addr_bytes[0], 4);
        assert_eq!(&addr_bytes[1..5], &[127, 0, 0, 1]);
        assert_eq!(&addr_bytes[5..7], &55943u16.to_be_bytes());
    }

    #[test]
    fn full_handshake_reaches_disconnect() {
        let client_addr: SocketAddr = "127.0.0.1:55943".parse().unwrap();
        let mut session = new_session();

        // ConnectionRequest
        let mut cr = vec![ID_CONNECTION_REQUEST];
        cr.extend_from_slice(&123i64.to_be_bytes()); // client guid
        cr.extend_from_slice(&456i64.to_be_bytes()); // timestamp
        cr.push(0);
        let datagram = wrap_test_datagram(&cr);
        let replies = handle_datagram(&mut session, &datagram, client_addr, 1, "line1", "line2");
        assert_eq!(
            replies.len(),
            2,
            "expected an ACK plus the ConnectionRequestAccepted reply"
        );
        assert_eq!(replies[0][0], 0xC0, "first reply should be the ACK");
        assert_eq!(session.stage, Stage::AwaitingNewIncomingConnection);

        // NewIncomingConnection
        let nic = vec![ID_NEW_INCOMING_CONNECTION];
        let datagram = wrap_test_datagram(&nic);
        let replies = handle_datagram(&mut session, &datagram, client_addr, 1, "line1", "line2");
        assert_eq!(
            replies.len(),
            1,
            "just the ACK, no protocol reply needed here"
        );
        assert_eq!(session.stage, Stage::AwaitingNetworkSettingsRequest);

        // RequestNetworkSettings, wrapped in a 0xFE batch (observed real
        // client behavior: this is never sent raw/unwrapped).
        let mut rns_packet = MC_ID_REQUEST_NETWORK_SETTINGS.to_vec();
        rns_packet.extend_from_slice(&766i32.to_be_bytes());
        let mut rns_batch = vec![MC_BATCH_HEADER];
        rns_batch.extend_from_slice(&encode_varint(rns_packet.len() as u32));
        rns_batch.extend_from_slice(&rns_packet);
        let datagram = wrap_test_datagram(&rns_batch);
        let replies = handle_datagram(&mut session, &datagram, client_addr, 1, "line1", "line2");
        assert_eq!(
            replies.len(),
            2,
            "expected an ACK plus the NetworkSettings reply"
        );
        assert_eq!(session.stage, Stage::AwaitingLoginAttempt);

        // A stray ConnectedPing keep-alive shouldn't be mistaken for a login attempt.
        let ping = vec![ID_CONNECTED_PING, 0, 0, 0, 0, 0, 0, 0, 0];
        let datagram = wrap_test_datagram(&ping);
        let replies = handle_datagram(&mut session, &datagram, client_addr, 1, "line1", "line2");
        assert_eq!(
            replies.len(),
            1,
            "just the ACK; ConnectedPing must not trigger Disconnect"
        );
        assert_eq!(session.stage, Stage::AwaitingLoginAttempt);

        // Anything else now triggers Disconnect.
        let login_stub = vec![MC_BATCH_HEADER, 0, 0, 0];
        let datagram = wrap_test_datagram(&login_stub);
        let replies = handle_datagram(
            &mut session,
            &datagram,
            client_addr,
            1,
            "offline line1",
            "offline line2",
        );
        assert_eq!(
            replies.len(),
            2,
            "expected an ACK plus the Disconnect reply"
        );
        assert!(session.is_done());

        let disconnect_datagram = &replies[1];
        // datagram header(4) + frame header(1+2+3+4=10) = 14 bytes before payload
        let payload = &disconnect_datagram[14..];
        assert_eq!(payload[0], MC_BATCH_HEADER);
    }

    /// Real `Login` packets carry several KB of JWT chain/skin data and
    /// arrive RakNet-fragmented, which our parser doesn't reassemble. This
    /// is the actual path a live client takes, not just the small
    /// single-frame login_stub used above.
    fn fragmented_test_datagram(split_count: u32, split_id: u16, split_index: u32) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push((RELIABLE_ORDERED << 5) | 0x10);
        frame.extend_from_slice(&800u16.to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0]);
        frame.extend_from_slice(&[0, 0, 0]);
        frame.push(0);
        frame.extend_from_slice(&split_count.to_be_bytes());
        frame.extend_from_slice(&split_id.to_be_bytes());
        frame.extend_from_slice(&split_index.to_be_bytes());
        frame.extend_from_slice(&[0xAB; 100]);

        let mut datagram = vec![0x8c, 0, 0, 0];
        datagram.extend_from_slice(&frame);
        datagram
    }

    #[test]
    fn fragment_tracker_rejects_zero_and_excessive_counts() {
        let mut session = new_session();

        let _ = session.record_fragment(1, 0, 0);
        assert!(session.pending_split.is_none());
        let _ = session.record_fragment(1, MAX_TRACKED_LOGIN_FRAGMENTS + 1, 0);
        assert!(session.pending_split.is_none());
    }

    #[test]
    fn fragment_tracker_rejects_out_of_range_indexes_without_refreshing_session() {
        let client_addr: SocketAddr = "127.0.0.1:55943".parse().unwrap();
        let mut session = new_session();
        session.stage = Stage::AwaitingLoginAttempt;
        session.last_activity = Instant::now() - Duration::from_secs(1);
        let previous_activity = session.last_activity;

        let datagram = fragmented_test_datagram(4, 1, 4);
        let _ = handle_datagram(&mut session, &datagram, client_addr, 1, "line1", "line2");

        assert!(session.pending_split.is_none());
        assert_eq!(session.last_activity, previous_activity);
    }

    #[test]
    fn fragment_tracker_resets_inconsistent_count_for_same_split_id() {
        let mut session = new_session();

        let _ = session.record_fragment(1, 4, 0);
        let _ = session.record_fragment(1, 5, 1);

        assert!(session.pending_split.is_none());
    }

    #[test]
    fn fragment_tracker_resets_count_mismatch_even_with_out_of_range_index() {
        let mut session = new_session();

        let _ = session.record_fragment(1, 4, 0);
        let _ = session.record_fragment(1, 5, 5);

        assert!(session.pending_split.is_none());
    }

    #[test]
    fn truncated_fragment_header_is_ignored_without_refresh_or_disconnect() {
        let client_addr: SocketAddr = "127.0.0.1:55943".parse().unwrap();
        let mut session = new_session();
        session.stage = Stage::AwaitingLoginAttempt;
        session.last_activity = Instant::now() - Duration::from_secs(1);
        let previous_activity = session.last_activity;
        let truncated = vec![0x8c, 0, 0, 0, (RELIABLE_ORDERED << 5) | 0x10];

        let replies = handle_datagram(&mut session, &truncated, client_addr, 1, "line1", "line2");

        assert_eq!(replies.len(), 1, "only the ACK should be sent");
        assert!(!session.is_done());
        assert_eq!(session.last_activity, previous_activity);
    }

    #[test]
    fn fragment_tracker_never_tracks_more_than_conservative_limit() {
        let mut session = new_session();
        for split_index in 0..MAX_TRACKED_LOGIN_FRAGMENTS {
            let _ = session.record_fragment(1, MAX_TRACKED_LOGIN_FRAGMENTS, split_index);
        }
        for split_index in MAX_TRACKED_LOGIN_FRAGMENTS..MAX_TRACKED_LOGIN_FRAGMENTS + 32 {
            let _ = session.record_fragment(1, MAX_TRACKED_LOGIN_FRAGMENTS, split_index);
        }

        let (_, _, seen) = session.pending_split.as_ref().unwrap();
        assert_eq!(seen.len(), MAX_TRACKED_LOGIN_FRAGMENTS as usize);
    }

    #[test]
    fn fragmented_login_waits_for_all_fragments_before_disconnect() {
        let client_addr: SocketAddr = "127.0.0.1:55943".parse().unwrap();
        let mut session = new_session();
        session.stage = Stage::AwaitingLoginAttempt;

        // First 3 of 4 fragments: must NOT disconnect yet -- doing so would
        // kick the client mid-send of its own Login packet.
        for split_index in 0..3 {
            let datagram = fragmented_test_datagram(4, 1, split_index);
            let replies =
                handle_datagram(&mut session, &datagram, client_addr, 1, "line1", "line2");
            assert_eq!(
                replies.len(),
                1,
                "just the ACK while fragments are still arriving"
            );
            assert!(!session.is_done());
        }

        // Final fragment completes the split -- now it's safe to disconnect.
        let datagram = fragmented_test_datagram(4, 1, 3);
        let replies = handle_datagram(&mut session, &datagram, client_addr, 1, "line1", "line2");
        assert_eq!(
            replies.len(),
            2,
            "expected an ACK plus the Disconnect reply"
        );
        assert!(session.is_done());
    }

    fn wrap_test_datagram(payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push(RELIABLE_ORDERED << 5);
        frame.extend_from_slice(&((payload.len() as u16) * 8).to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0]); // reliable index
        frame.extend_from_slice(&[0, 0, 0]); // order index
        frame.push(0); // order channel
        frame.extend_from_slice(payload);

        let mut datagram = vec![0x84, 0, 0, 0];
        datagram.extend_from_slice(&frame);
        datagram
    }

    #[test]
    fn varint_roundtrip() {
        for value in [0u32, 1, 127, 128, 300, 70000] {
            let encoded = encode_varint(value);
            let mut decoded: u32 = 0;
            let mut shift = 0;
            for byte in &encoded {
                decoded |= ((byte & 0x7F) as u32) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            assert_eq!(decoded, value);
        }
    }
}
