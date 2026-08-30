//! Wire protocol for the v2 message transport.
//!
//! Defines a minimal internal protocol with three frame types:
//!
//! | Type | Code | Purpose |
//! |------|------|---------|
//! | [`FRAME_DATA`] | 0 | Application payload |
//! | [`FRAME_CREDIT`] | 1 | Return receive credits to sender |
//! | [`FRAME_HELLO`] | 2 | Readiness/capability exchange during connect |
//!
//! # Frame Layout
//!
//! Every frame starts with a 12-byte header:
//!
//! ```text
//! Offset  Size  Field
//! 0       4     magic     (0x52444D41, "RDMA" LE)
//! 4       1     version   (1)
//! 5       1     frame_type (DATA=0, CREDIT=1, HELLO=2)
//! 6       2     reserved  (0)
//! 8       4     payload_len
//! ```
//!
//! Followed by a type-specific payload:
//!
//! - **DATA**: `payload_len` bytes of application data
//! - **CREDIT**: 4 bytes — `credits: u32` (LE)
//! - **HELLO**: 12 bytes — `data_recv_capacity: u32`, `max_message_size: u32`,
//!   `protocol_version: u32` (all LE)
//!
//! # Validation
//!
//! [`parse_header`] validates magic, version, and frame type.
//! [`parse_hello`] / [`parse_credit`] validate payload lengths.
//! Protocol violations produce [`Error::ProtocolViolation`].

use super::error::{Error, Result};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Protocol magic number: "RDMA" in little-endian.
pub const PROTO_MAGIC: u32 = 0x52444D41;

/// Current protocol version.
pub const PROTO_VERSION: u8 = 1;

/// Frame header size in bytes.
pub const HEADER_SIZE: usize = 12;

/// Frame type: application data.
pub const FRAME_DATA: u8 = 0;

/// Frame type: credit return.
pub const FRAME_CREDIT: u8 = 1;

/// Frame type: readiness/hello exchange.
pub const FRAME_HELLO: u8 = 2;

/// CREDIT payload size in bytes.
pub const CREDIT_PAYLOAD_SIZE: usize = 4;

/// HELLO payload size in bytes.
pub const HELLO_PAYLOAD_SIZE: usize = 12;

/// Total CREDIT frame size (header + payload).
pub const CREDIT_FRAME_SIZE: usize = HEADER_SIZE + CREDIT_PAYLOAD_SIZE;

/// Total HELLO frame size (header + payload).
pub const HELLO_FRAME_SIZE: usize = HEADER_SIZE + HELLO_PAYLOAD_SIZE;

/// Minimum size for a control receive buffer (fits any control frame).
pub const CTRL_BUF_SIZE: usize = HELLO_FRAME_SIZE; // 24 bytes — largest control frame

/// Number of dedicated control receive buffers.
pub const CTRL_RECV_COUNT: usize = 2;

/// Number of control send MRs.
pub const CTRL_SEND_COUNT: usize = 2;

// ─── Parsed Types ────────────────────────────────────────────────────────────

/// Parsed frame header.
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub frame_type: u8,
    pub payload_len: u32,
}

/// Parsed HELLO payload.
#[derive(Debug, Clone, Copy)]
pub struct HelloPayload {
    /// Number of data receive buffers the sender has posted.
    pub data_recv_capacity: u32,
    /// Maximum message (payload) size the sender supports.
    pub max_message_size: u32,
    /// Protocol version (currently 1).
    pub protocol_version: u32,
}

/// Parsed CREDIT payload.
#[derive(Debug, Clone, Copy)]
pub struct CreditPayload {
    /// Number of credits being returned.
    pub credits: u32,
}

// ─── Serialization ───────────────────────────────────────────────────────────

/// Write a frame header into `buf`. Returns the header size (always [`HEADER_SIZE`]).
///
/// # Panics
///
/// Panics if `buf.len() < HEADER_SIZE`.
fn write_header(buf: &mut [u8], frame_type: u8, payload_len: u32) {
    assert!(buf.len() >= HEADER_SIZE);
    buf[0..4].copy_from_slice(&PROTO_MAGIC.to_le_bytes());
    buf[4] = PROTO_VERSION;
    buf[5] = frame_type;
    buf[6..8].copy_from_slice(&[0u8; 2]); // reserved
    buf[8..12].copy_from_slice(&payload_len.to_le_bytes());
}

/// Write a DATA frame (header + payload) into `buf`.
///
/// Returns the total frame size (header + data length).
///
/// # Panics
///
/// Panics if `buf` is too small for the header + data.
pub fn write_data_frame(buf: &mut [u8], data: &[u8]) -> usize {
    let total = HEADER_SIZE + data.len();
    assert!(buf.len() >= total);
    write_header(buf, FRAME_DATA, data.len() as u32);
    buf[HEADER_SIZE..total].copy_from_slice(data);
    total
}

/// Write a CREDIT frame into `buf`.
///
/// Returns the total frame size ([`CREDIT_FRAME_SIZE`]).
///
/// # Panics
///
/// Panics if `buf.len() < CREDIT_FRAME_SIZE`.
pub fn write_credit_frame(buf: &mut [u8], credits: u32) -> usize {
    assert!(buf.len() >= CREDIT_FRAME_SIZE);
    write_header(buf, FRAME_CREDIT, CREDIT_PAYLOAD_SIZE as u32);
    buf[HEADER_SIZE..CREDIT_FRAME_SIZE].copy_from_slice(&credits.to_le_bytes());
    CREDIT_FRAME_SIZE
}

/// Write a HELLO frame into `buf`.
///
/// Returns the total frame size ([`HELLO_FRAME_SIZE`]).
///
/// # Panics
///
/// Panics if `buf.len() < HELLO_FRAME_SIZE`.
pub fn write_hello_frame(buf: &mut [u8], data_recv_capacity: u32, max_message_size: u32) -> usize {
    assert!(buf.len() >= HELLO_FRAME_SIZE);
    write_header(buf, FRAME_HELLO, HELLO_PAYLOAD_SIZE as u32);
    let p = HEADER_SIZE;
    buf[p..p + 4].copy_from_slice(&data_recv_capacity.to_le_bytes());
    buf[p + 4..p + 8].copy_from_slice(&max_message_size.to_le_bytes());
    buf[p + 8..p + 12].copy_from_slice(&(PROTO_VERSION as u32).to_le_bytes());
    HELLO_FRAME_SIZE
}

// ─── Parsing ─────────────────────────────────────────────────────────────────

/// Parse and validate a frame header from `buf`.
///
/// `received_len` is the total bytes received (from the CQE `byte_len`).
/// The caller must ensure `buf.len() >= received_len`; this is always true
/// when `buf` is an MR slice and `received_len` comes from a CQE.
///
/// # Errors
///
/// Returns [`Error::ProtocolViolation`] on:
/// - Insufficient bytes for header
/// - Bad magic number
/// - Unsupported version
/// - Unknown frame type
/// - Payload length exceeds received data
pub fn parse_header(buf: &[u8], received_len: usize) -> Result<FrameHeader> {
    debug_assert!(
        buf.len() >= received_len,
        "buf.len() ({}) < received_len ({received_len})",
        buf.len()
    );
    if received_len < HEADER_SIZE {
        return Err(Error::ProtocolViolation(format!(
            "frame too short: {received_len} < {HEADER_SIZE}"
        )));
    }

    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != PROTO_MAGIC {
        return Err(Error::ProtocolViolation(format!(
            "bad magic: {magic:#010x}, expected {PROTO_MAGIC:#010x}"
        )));
    }

    let version = buf[4];
    if version != PROTO_VERSION {
        return Err(Error::ProtocolViolation(format!(
            "unsupported version: {version}, expected {PROTO_VERSION}"
        )));
    }

    let frame_type = buf[5];
    if frame_type > FRAME_HELLO {
        return Err(Error::ProtocolViolation(format!(
            "unknown frame type: {frame_type}"
        )));
    }

    let payload_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let total = HEADER_SIZE + payload_len as usize;
    if total > received_len {
        return Err(Error::ProtocolViolation(format!(
            "payload extends past received data: header says {total}, got {received_len}"
        )));
    }

    Ok(FrameHeader {
        frame_type,
        payload_len,
    })
}

/// Parse a HELLO payload from `payload` (bytes after the header).
///
/// Validates payload length and protocol version match.
///
/// # Errors
///
/// Returns [`Error::ProtocolViolation`] if the payload is too short
/// or the peer's protocol version does not match [`PROTO_VERSION`].
pub fn parse_hello(payload: &[u8]) -> Result<HelloPayload> {
    if payload.len() < HELLO_PAYLOAD_SIZE {
        return Err(Error::ProtocolViolation(format!(
            "HELLO payload too short: {} < {HELLO_PAYLOAD_SIZE}",
            payload.len()
        )));
    }
    let data_recv_capacity = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let max_message_size = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let protocol_version = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);

    if protocol_version != PROTO_VERSION as u32 {
        return Err(Error::ProtocolViolation(format!(
            "HELLO version mismatch: peer={protocol_version}, local={PROTO_VERSION}"
        )));
    }

    Ok(HelloPayload {
        data_recv_capacity,
        max_message_size,
        protocol_version,
    })
}

/// Parse a CREDIT payload from `payload` (bytes after the header).
///
/// # Errors
///
/// Returns [`Error::ProtocolViolation`] if the payload is too short.
pub fn parse_credit(payload: &[u8]) -> Result<CreditPayload> {
    if payload.len() < CREDIT_PAYLOAD_SIZE {
        return Err(Error::ProtocolViolation(format!(
            "CREDIT payload too short: {} < {CREDIT_PAYLOAD_SIZE}",
            payload.len()
        )));
    }
    let credits = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(CreditPayload { credits })
}

/// Compute the required data recv MR size for a given max message payload.
///
/// The MR must be large enough for the protocol header plus the full payload.
#[inline]
pub fn data_mr_size(max_payload: usize) -> usize {
    HEADER_SIZE + max_payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_frame_roundtrip() {
        let payload = b"hello world";
        let mut buf = vec![0u8; HEADER_SIZE + payload.len()];
        let len = write_data_frame(&mut buf, payload);
        assert_eq!(len, HEADER_SIZE + payload.len());

        let header = parse_header(&buf, len).unwrap();
        assert_eq!(header.frame_type, FRAME_DATA);
        assert_eq!(header.payload_len, payload.len() as u32);
        assert_eq!(&buf[HEADER_SIZE..len], payload);
    }

    #[test]
    fn test_credit_frame_roundtrip() {
        let mut buf = vec![0u8; CREDIT_FRAME_SIZE];
        let len = write_credit_frame(&mut buf, 42);
        assert_eq!(len, CREDIT_FRAME_SIZE);

        let header = parse_header(&buf, len).unwrap();
        assert_eq!(header.frame_type, FRAME_CREDIT);
        let credit = parse_credit(&buf[HEADER_SIZE..]).unwrap();
        assert_eq!(credit.credits, 42);
    }

    #[test]
    fn test_hello_frame_roundtrip() {
        let mut buf = vec![0u8; HELLO_FRAME_SIZE];
        let len = write_hello_frame(&mut buf, 32, 65536);
        assert_eq!(len, HELLO_FRAME_SIZE);

        let header = parse_header(&buf, len).unwrap();
        assert_eq!(header.frame_type, FRAME_HELLO);
        let hello = parse_hello(&buf[HEADER_SIZE..]).unwrap();
        assert_eq!(hello.data_recv_capacity, 32);
        assert_eq!(hello.max_message_size, 65536);
        assert_eq!(hello.protocol_version, PROTO_VERSION as u32);
    }

    #[test]
    fn test_zero_length_data_frame() {
        let mut buf = vec![0u8; HEADER_SIZE];
        let len = write_data_frame(&mut buf, b"");
        assert_eq!(len, HEADER_SIZE);

        let header = parse_header(&buf, len).unwrap();
        assert_eq!(header.frame_type, FRAME_DATA);
        assert_eq!(header.payload_len, 0);
    }

    #[test]
    fn test_bad_magic() {
        let mut buf = vec![0u8; HEADER_SIZE];
        write_data_frame(&mut buf, b"");
        buf[0] = 0xFF; // corrupt magic
        let result = parse_header(&buf, HEADER_SIZE);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bad magic"));
    }

    #[test]
    fn test_bad_version() {
        let mut buf = vec![0u8; HEADER_SIZE];
        write_data_frame(&mut buf, b"");
        buf[4] = 99; // bad version
        let result = parse_header(&buf, HEADER_SIZE);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version"));
    }

    #[test]
    fn test_unknown_frame_type() {
        let mut buf = vec![0u8; HEADER_SIZE];
        write_data_frame(&mut buf, b"");
        buf[5] = 99; // unknown type
        let result = parse_header(&buf, HEADER_SIZE);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("frame type"));
    }

    #[test]
    fn test_truncated_header() {
        let buf = vec![0u8; 4]; // too short
        let result = parse_header(&buf, 4);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_payload_exceeds_received() {
        let mut buf = vec![0u8; HEADER_SIZE + 10];
        write_data_frame(&mut buf, &[0u8; 10]);
        // Claim we only received the header (not the payload)
        let result = parse_header(&buf, HEADER_SIZE);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("extends past"));
    }

    #[test]
    fn test_data_mr_size() {
        assert_eq!(data_mr_size(0), HEADER_SIZE);
        assert_eq!(data_mr_size(1024), HEADER_SIZE + 1024);
        assert_eq!(data_mr_size(65536), HEADER_SIZE + 65536);
    }
}
