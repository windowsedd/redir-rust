use std::io;
use std::io::Cursor;
use std::net::SocketAddr;

use async_trait::async_trait;

use tokio::net::TcpStream;

/// Direction of a data chunk relative to the backend target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToTarget,
    TargetToClient,
}

/// Context handed to a plugin after every configured backend target failed.
#[derive(Debug, Clone)]
pub struct FailureContext {
    pub client_addr: SocketAddr,
    pub listen_addr: SocketAddr,
    /// The final target attempted after exhausting the ordered target list.
    pub target_addr: SocketAddr,
    /// The connection error from the final target attempt.
    pub error: String,
}

/// Outcome of a plugin's on_target_failure hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction {
    /// The plugin fully handled the client socket.
    Handled,
    /// The plugin did not handle this failure.
    PassThrough,
}

/// What a client does on connect, as inferred from the Handshake packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// Minecraft Server List Ping (next_state = status).
    StatusPing,
    /// Minecraft Join Attempt (next_state = login).
    LoginAttempt,
}

impl std::fmt::Display for PacketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketKind::StatusPing => write!(f, "ping"),
            PacketKind::LoginAttempt => write!(f, "join"),
        }
    }
}

/// Maximum number of bytes a VarInt can encode (5 bytes for i32).
const MAX_VARINT_BYTES: u32 = 5;

/// Read a VarInt from a slice cursor.
pub fn read_varint_from_slice(cursor: &mut Cursor<&[u8]>) -> io::Result<i32> {
    let mut num_read = 0u32;
    let mut result: i32 = 0;
    loop {
        if num_read >= MAX_VARINT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VarInt too long",
            ));
        }
        let mut byte = [0u8; 1];
        std::io::Read::read_exact(cursor, &mut byte)?;
        result |= ((byte[0] & 0b0111_1111) as i32) << (7 * num_read);
        num_read += 1;
        if byte[0] & 0b1000_0000 == 0 {
            break;
        }
    }
    Ok(result)
}

/// Parse a Handshake packet body: (protocol_version, next_state).
pub fn parse_handshake(body: &[u8]) -> io::Result<(i32, i32)> {
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
    std::io::Read::read_exact(&mut cursor, &mut addr_buf)?;

    let mut port_buf = [0u8; 2];
    std::io::Read::read_exact(&mut cursor, &mut port_buf)?;

    let next_state = read_varint_from_slice(&mut cursor)?;
    Ok((protocol_version, next_state))
}

/// Detect if a consumed packet is a Minecraft Handshake and return its kind.
fn detect_handshake_kind(packet: &[u8]) -> Option<PacketKind> {
    let mut cursor = Cursor::new(packet);
    let _len = match read_varint_from_slice(&mut cursor) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let packet_id = match read_varint_from_slice(&mut cursor) {
        Ok(v) => v,
        Err(_) => return None,
    };
    if packet_id != 0x00 {
        return None;
    }

    let body = &packet[cursor.position() as usize..];
    match parse_handshake(body) {
        Ok((_, next_state)) => match next_state {
            1 => Some(PacketKind::StatusPing),
            2 => Some(PacketKind::LoginAttempt),
            _ => None,
        },
        Err(_) => None,
    }
}

/// Probe the first packet from the client without consuming bytes.
///
/// Uses peek so the bytes stay in the socket buffer for later piping.
/// If the packet is a valid Minecraft Handshake (packet_id == 0), returns
/// (Some(kind), true) — the caller should **NOT** consume bytes (they
/// stay in the buffer).  For any other traffic returns (None, _).
pub async fn probe_first_packet(stream: &mut TcpStream) -> (Option<PacketKind>, bool, bool) {
    // (packet_kind, is_minecraft, should_consume)
    // Read up to 512 bytes via peek to decode the VarInt length and packet.
    let mut peek_buf = [0u8; 512];
    let mut peek_len = match tokio::time::timeout(
        std::time::Duration::from_millis(200),
        stream.peek(&mut peek_buf),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 => n,
        _ => return (None, false, false),
    };

    // Decode length VarInt.
    let mut cursor = Cursor::new(&peek_buf[..peek_len]);
    let pkt_len = match read_varint_from_slice(&mut cursor) {
        Ok(v) => v,
        Err(_) => return (None, false, false),
    };
    if !(1..=4096).contains(&pkt_len) {
        return (None, false, false);
    }

    // Need the full packet in the peek buffer to parse it.
    let total_needed = peek_len + pkt_len as usize;
    if peek_len < total_needed {
        // More data needed — keep peeping until we have it or timeout.
        let mut peek_pos = peek_len;
        while peek_pos < total_needed {
            let _remaining = total_needed - peek_pos;
            let n = match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                stream.peek(&mut peek_buf[peek_pos..]),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => n,
                _ => break,
            };
            peek_pos += n;
        }
        peek_len = peek_pos.min(total_needed);
    }

    let full_len = peek_len.min(peek_len);
    if let Some(kind) = detect_handshake_kind(&peek_buf[..full_len]) {
        return (Some(kind), true, true);
    }
    (None, false, false)
}

/// A hook into the lifecycle of a proxied connection.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;

    fn applies_to(&self, listen_addr: SocketAddr) -> bool {
        let _ = listen_addr;
        true
    }

    /// Called as soon as a client connects, before any attempt to reach the backend.
    /// Returning alse rejects the connection immediately.
    async fn on_connect(&self, client_addr: SocketAddr) -> bool {
        let _ = client_addr;
        true
    }

    /// Called for each chunk of data before it is forwarded.
    async fn on_data_receive(&self, direction: Direction, data: &[u8]) -> Option<Vec<u8>> {
        let _ = (direction, data);
        None
    }

    /// Called once after connecting to every configured backend target failed.
    async fn on_target_failure(
        &self,
        ctx: &FailureContext,
        client: &mut TcpStream,
    ) -> FailureAction {
        let _ = (ctx, client);
        FailureAction::PassThrough
    }
}
