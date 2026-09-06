use std::io::{self, Cursor, Read};
use std::net::SocketAddr;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing::debug;

use crate::plugin::{FailureAction, FailureContext, Plugin};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PACKET_LEN: i32 = 1 << 20;

/// Built-in 64x64 "offline scroll" icon, embedded at compile time so the
/// plugin has a sensible favicon out of the box.
const DEFAULT_ICON_BYTES: &[u8] = include_bytes!("../../assets/offline_icon.png");

fn default_status_line() -> String {
    "\u{00A7}c伺服器無法連線！ / Server unreachable!".to_string()
}

fn default_motd_line1() -> String {
    "\u{00A7}c伺服器無法連線！ / Server unreachable!".to_string()
}

fn default_motd_line2() -> String {
    "\u{00A7}e請稍後再試 / Please Try Again".to_string()
}

/// Responds to Minecraft's Server List Ping protocol with a canned "offline"
/// status when the real backend target is unreachable.
pub struct MinecraftOfflinePlugin {
    listen_port: u16,
    status_line: String,
    motd_line1: String,
    motd_line2: String,
    favicon_base64: Option<String>,
}

impl MinecraftOfflinePlugin {
    /// Builds a plugin with the default bilingual "server unreachable" copy
    /// and the built-in scroll icon as favicon.
    pub fn new(listen_port: u16) -> Self {
        Self {
            listen_port,
            status_line: default_status_line(),
            motd_line1: default_motd_line1(),
            motd_line2: default_motd_line2(),
            favicon_base64: Some(BASE64.encode(DEFAULT_ICON_BYTES)),
        }
    }

    /// Overrides the text shown top-right in the server list (the "version"
    /// field). Combined with `protocol: -1`, clients render this string in
    /// place of a player count.
    pub fn with_status_line(mut self, status_line: impl Into<String>) -> Self {
        self.status_line = status_line.into();
        self
    }

    /// Overrides the two MOTD lines shown under the server name.
    pub fn with_motd(mut self, line1: impl Into<String>, line2: impl Into<String>) -> Self {
        self.motd_line1 = line1.into();
        self.motd_line2 = line2.into();
        self
    }

    /// Overrides the favicon with raw PNG bytes (must be 64x64 to render
    /// correctly in vanilla clients). Pass `None` to omit the favicon field.
    pub fn with_favicon_png_bytes(mut self, png_bytes: Option<&[u8]>) -> Self {
        self.favicon_base64 = png_bytes.map(|bytes| BASE64.encode(bytes));
        self
    }
}

#[async_trait]
impl Plugin for MinecraftOfflinePlugin {
    fn name(&self) -> &str {
        "minecraft-offline-motd"
    }

    fn applies_to(&self, listen_addr: SocketAddr) -> bool {
        listen_addr.port() == self.listen_port
    }

    async fn on_target_failure(
        &self,
        ctx: &FailureContext,
        client: &mut TcpStream,
    ) -> FailureAction {
        let status = build_status_json(
            &self.status_line,
            &self.motd_line1,
            &self.motd_line2,
            self.favicon_base64.as_deref(),
        );
        let kick_text = format!("{}\n{}", self.motd_line1, self.motd_line2);

        match timeout(HANDSHAKE_TIMEOUT, handle_slp(client, &status, &kick_text)).await {
            Ok(Ok(())) => FailureAction::Handled,
            Ok(Err(err)) => {
                debug!(client = %ctx.client_addr, %err, "SLP handling failed, falling back");
                FailureAction::PassThrough
            }
            Err(_) => {
                debug!(client = %ctx.client_addr, "SLP handshake timed out");
                FailureAction::PassThrough
            }
        }
    }
}

fn build_status_json(
    status_line: &str,
    motd_line1: &str,
    motd_line2: &str,
    favicon_base64: Option<&str>,
) -> String {
    let mut status = json!({
        "version": { "name": status_line, "protocol": -1 },
        "players": { "max": 0, "online": 0, "sample": [] },
        "description": { "text": format!("{motd_line1}\n{motd_line2}") },
    });

    if let Some(favicon) = favicon_base64 {
        status["favicon"] = json!(format!("data:image/png;base64,{favicon}"));
    }

    status.to_string()
}

/// Max encoded length of an i32 VarInt: 5 groups of 7 bits (35 bits of
/// capacity for 32 bits of value). `num_read` must never reach 5 while a
/// continuation bit is still set -- a 6th group would shift by 7*5=35,
/// which overflows i32's 32-bit width and panics.
const MAX_VARINT_BYTES: u32 = 5;

async fn read_varint(stream: &mut TcpStream) -> io::Result<i32> {
    let mut num_read = 0u32;
    let mut result: i32 = 0;
    loop {
        let byte = stream.read_u8().await?;
        result |= ((byte & 0b0111_1111) as i32) << (7 * num_read);
        num_read += 1;
        if byte & 0b1000_0000 == 0 {
            break;
        }
        if num_read >= MAX_VARINT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VarInt too long",
            ));
        }
    }
    Ok(result)
}

fn read_varint_from_slice(cursor: &mut Cursor<&[u8]>) -> io::Result<i32> {
    let mut num_read = 0u32;
    let mut result: i32 = 0;
    loop {
        let mut byte = [0u8; 1];
        Read::read_exact(cursor, &mut byte)?;
        result |= ((byte[0] & 0b0111_1111) as i32) << (7 * num_read);
        num_read += 1;
        if byte[0] & 0b1000_0000 == 0 {
            break;
        }
        if num_read >= MAX_VARINT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VarInt too long",
            ));
        }
    }
    Ok(result)
}

fn write_varint(buf: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    loop {
        let mut byte = (value & 0b0111_1111) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0b1000_0000;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Reads a length-prefixed Minecraft packet and returns (packet_id, remaining payload).
async fn read_packet(stream: &mut TcpStream) -> io::Result<(i32, Vec<u8>)> {
    let len = read_varint(stream).await?;
    if !(0..=MAX_PACKET_LEN).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid packet length",
        ));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;

    let mut cursor = Cursor::new(body.as_slice());
    let packet_id = read_varint_from_slice(&mut cursor)?;
    let start = cursor.position() as usize;
    Ok((packet_id, body[start..].to_vec()))
}

fn frame_packet(packet_id: i32, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 5);
    write_varint(&mut body, packet_id);
    body.extend_from_slice(payload);

    let mut framed = Vec::with_capacity(body.len() + 5);
    write_varint(&mut framed, body.len() as i32);
    framed.extend_from_slice(&body);
    framed
}

/// Parses a Handshake packet body (protocol_version: VarInt, server_address:
/// String, server_port: u16, next_state: VarInt) and returns
/// `(protocol_version, next_state)`.
fn parse_handshake(body: &[u8]) -> io::Result<(i32, i32)> {
    let mut cursor = Cursor::new(body);
    let protocol_version = read_varint_from_slice(&mut cursor)?;

    let addr_len = read_varint_from_slice(&mut cursor)?;
    if addr_len < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid server address length",
        ));
    }
    let mut addr_buf = vec![0u8; addr_len as usize];
    Read::read_exact(&mut cursor, &mut addr_buf)?;

    let mut port_buf = [0u8; 2];
    Read::read_exact(&mut cursor, &mut port_buf)?;

    let next_state = read_varint_from_slice(&mut cursor)?;
    Ok((protocol_version, next_state))
}

/// Builds the payload for a Login Disconnect packet (id 0x00), whose sole
/// field is a VarInt-length-prefixed JSON chat component in every supported
/// protocol version. NBT components are used by disconnect packets in later
/// protocol states, but not by Login Disconnect.
fn build_login_disconnect_payload(kick_text: &str) -> Vec<u8> {
    let json = json!({ "text": kick_text }).to_string();
    let mut payload = Vec::with_capacity(json.len() + 5);
    write_varint(&mut payload, json.len() as i32);
    payload.extend_from_slice(json.as_bytes());
    payload
}

async fn handle_slp(stream: &mut TcpStream, status_json: &str, kick_text: &str) -> io::Result<()> {
    // Handshake packet (id 0x00). Its `next_state` field distinguishes a
    // server-list status ping (1) from an actual join attempt (2, login).
    let (packet_id, handshake_body) = read_packet(stream).await?;
    if packet_id != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected handshake packet",
        ));
    }

    let (_, next_state) = parse_handshake(&handshake_body)?;
    match next_state {
        1 => handle_status(stream, status_json).await,
        2 => handle_login(stream, kick_text).await,
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("handshake requested next_state={next_state}, not status or login"),
        )),
    }
}

async fn handle_status(stream: &mut TcpStream, status_json: &str) -> io::Result<()> {
    // Status request packet (id 0x00, empty body).
    let (packet_id, _) = read_packet(stream).await?;
    if packet_id != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected status request packet",
        ));
    }

    let mut status_payload = Vec::with_capacity(status_json.len() + 5);
    write_varint(&mut status_payload, status_json.len() as i32);
    status_payload.extend_from_slice(status_json.as_bytes());
    stream
        .write_all(&frame_packet(0x00, &status_payload))
        .await?;

    // Optional ping (id 0x01): echo the payload back if the client sends one.
    if let Ok((packet_id, payload)) = read_packet(stream).await {
        if packet_id == 0x01 {
            let _ = stream.write_all(&frame_packet(0x01, &payload)).await;
        }
    }

    stream.shutdown().await.ok();
    Ok(())
}

/// Handles an actual join attempt: reads (and ignores) Login Start, then
/// kicks the client with our MOTD text instead of leaving them with a bare
/// connection failure.
async fn handle_login(stream: &mut TcpStream, kick_text: &str) -> io::Result<()> {
    let (packet_id, _) = read_packet(stream).await?;
    if packet_id != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected login start packet",
        ));
    }

    let payload = build_login_disconnect_payload(kick_text);
    stream.write_all(&frame_packet(0x00, &payload)).await?;
    stream.shutdown().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_varint_matches_known_encodings() {
        // Reference values from https://minecraft.wiki/w/Java_Edition_protocol/Packets#VarInt_and_VarLong
        let mut buf = Vec::new();
        write_varint(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        write_varint(&mut buf, 1);
        assert_eq!(buf, vec![0x01]);

        buf.clear();
        write_varint(&mut buf, 127);
        assert_eq!(buf, vec![0x7f]);

        buf.clear();
        write_varint(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);

        buf.clear();
        write_varint(&mut buf, 25565);
        assert_eq!(buf, vec![0xdd, 0xc7, 0x01]);

        buf.clear();
        write_varint(&mut buf, -1);
        assert_eq!(buf, vec![0xff, 0xff, 0xff, 0xff, 0x0f]);
    }

    #[test]
    fn read_varint_from_slice_roundtrips_write_varint() {
        for value in [0, 1, 127, 128, 25565, 2_147_483_647, -1, -2_147_483_648] {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);
            let mut cursor = Cursor::new(buf.as_slice());
            assert_eq!(read_varint_from_slice(&mut cursor).unwrap(), value);
        }
    }

    #[test]
    fn read_varint_from_slice_rejects_too_long_sequence() {
        let buf = vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        let mut cursor = Cursor::new(buf.as_slice());
        assert!(read_varint_from_slice(&mut cursor).is_err());
    }

    #[test]
    fn frame_packet_prefixes_id_and_length() {
        let framed = frame_packet(0x00, b"hi");
        // body = [packet_id varint][payload] = [0x00, b'h', b'i'] (len 3),
        // framed = [length varint][body] = [0x03, 0x00, b'h', b'i']
        assert_eq!(framed, vec![0x03, 0x00, b'h', b'i']);
    }

    #[test]
    fn build_status_json_includes_expected_fields() {
        let json = build_status_json("status", "line1", "line2", Some("b64data"));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["version"]["name"], "status");
        assert_eq!(value["version"]["protocol"], -1);
        assert_eq!(value["players"]["max"], 0);
        assert_eq!(value["players"]["online"], 0);
        assert_eq!(value["description"]["text"], "line1\nline2");
        assert_eq!(value["favicon"], "data:image/png;base64,b64data");
    }

    #[test]
    fn build_status_json_omits_favicon_when_none() {
        let json = build_status_json("status", "line1", "line2", None);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("favicon").is_none());
    }

    #[test]
    fn parse_handshake_extracts_protocol_version_and_next_state() {
        let mut body = Vec::new();
        write_varint(&mut body, 766); // protocol_version
        let addr = b"play.example.com";
        write_varint(&mut body, addr.len() as i32);
        body.extend_from_slice(addr);
        body.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut body, 1); // next_state: status

        let (protocol_version, next_state) = parse_handshake(&body).unwrap();
        assert_eq!(protocol_version, 766);
        assert_eq!(next_state, 1);
    }

    #[test]
    fn build_login_disconnect_payload_uses_length_prefixed_json() {
        let payload = build_login_disconnect_payload("kicked");
        let mut cursor = Cursor::new(payload.as_slice());
        let len = read_varint_from_slice(&mut cursor).unwrap() as usize;
        let start = cursor.position() as usize;
        assert_eq!(payload.len(), start + len);
        let json_bytes = &payload[start..start + len];
        let value: serde_json::Value = serde_json::from_slice(json_bytes).unwrap();
        assert_eq!(value["text"], "kicked");
    }
}
