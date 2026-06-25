//! Shared IPC protocol for single-instance mode: a later launch forwards the file it was
//! asked to open to the already-running instance.
//!
//! Transport (the Windows named pipe) lives in `fire`; this crate owns the *wire format* so
//! both sides agree byte-for-byte. It is deliberately dependency-free (no serde) so the
//! forward path adds as little startup cost as possible.
//!
//! Framing: one message = `u32` little-endian length prefix + payload.
//! Payload layout (v1):
//! ```text
//!   [u8  version]       protocol version (PROTOCOL_VERSION)
//!   [u8  window_mode]   WindowMode discriminant
//!   [u8  activate]      0 / 1
//!   [u8  reserved]      0
//!   [.. utf-8 path bytes]
//! ```
//! The `u32` length prefix counts the payload bytes only (not the 4 prefix bytes).

use std::io::{self, Read, Write};
use std::path::PathBuf;

/// Named pipe the running instance listens on and a forwarding launch connects to.
pub const PIPE_NAME: &str = r"\\.\pipe\fire";

/// Single-instance mutex name. `Local\` scope = per-login session (supports
/// fast-user-switching); we explicitly do NOT want one machine-wide instance.
pub const MUTEX_NAME: &str = r"Local\fire-singleton";

/// Protocol version byte, bumped on incompatible wire changes.
pub const PROTOCOL_VERSION: u8 = 1;

/// Upper bound on a single framed message payload (header + path). Guards against a
/// corrupt/hostile length prefix forcing a huge allocation.
pub const MAX_MESSAGE_LEN: u32 = 64 * 1024;

/// Fixed-size header that precedes the path bytes in a payload.
const HEADER_LEN: usize = 4;

/// How a newly-opened file should be placed into the running instance's window/session model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WindowMode {
    /// Use the configured default (reuse single window unless overridden).
    Default = 0,
    /// Force a brand-new window for this file.
    NewWindow = 1,
    /// Open as a new tab in the active window.
    NewTab = 2,
}

impl WindowMode {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(WindowMode::Default),
            1 => Some(WindowMode::NewWindow),
            2 => Some(WindowMode::NewTab),
            _ => None,
        }
    }
}

/// Flags carried alongside the path in an open request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags {
    pub window_mode: WindowMode,
    /// Whether the running instance should foreground/activate the target window.
    pub activate: bool,
}

impl Default for OpenFlags {
    fn default() -> Self {
        Self { window_mode: WindowMode::Default, activate: true }
    }
}

/// A request to open one file, sent forwarder → running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    pub path: PathBuf,
    pub flags: OpenFlags,
}

impl OpenRequest {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), flags: OpenFlags::default() }
    }

    /// Serialize the payload (header + utf-8 path) into a fresh buffer. Does NOT
    /// include the length prefix; [`write_message`] adds that.
    fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        let path_str = self
            .path
            .to_str()
            .ok_or(ProtocolError::NonUtf8Path)?;
        let path_bytes = path_str.as_bytes();
        let total = HEADER_LEN + path_bytes.len();
        if total > MAX_MESSAGE_LEN as usize {
            return Err(ProtocolError::TooLong(total));
        }
        let mut buf = Vec::with_capacity(total);
        buf.push(PROTOCOL_VERSION);
        buf.push(self.flags.window_mode as u8);
        buf.push(self.flags.activate as u8);
        buf.push(0); // reserved
        buf.extend_from_slice(path_bytes);
        Ok(buf)
    }

    /// Parse a payload (header + path bytes) produced by [`encode_payload`].
    fn decode_payload(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::Truncated);
        }
        let version = buf[0];
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let window_mode =
            WindowMode::from_u8(buf[1]).ok_or(ProtocolError::BadWindowMode(buf[1]))?;
        let activate = buf[2] != 0;
        // buf[3] reserved, ignored.
        let path_str =
            std::str::from_utf8(&buf[HEADER_LEN..]).map_err(|_| ProtocolError::NonUtf8Path)?;
        Ok(OpenRequest {
            path: PathBuf::from(path_str),
            flags: OpenFlags { window_mode, activate },
        })
    }
}

/// Errors from (de)serializing or framing a message.
#[derive(Debug)]
pub enum ProtocolError {
    /// Path was not valid UTF-8 (Windows paths are UTF-16 natively; we require the
    /// lossless UTF-8 form, which holds for all real filesystem paths here).
    NonUtf8Path,
    /// Encoded payload exceeds [`MAX_MESSAGE_LEN`].
    TooLong(usize),
    /// Declared length prefix exceeds [`MAX_MESSAGE_LEN`].
    LengthCap(u32),
    /// Payload ended before a full header was read.
    Truncated,
    /// Unknown protocol version byte.
    UnsupportedVersion(u8),
    /// Unknown WindowMode discriminant.
    BadWindowMode(u8),
    /// Underlying transport I/O error.
    Io(io::Error),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::NonUtf8Path => write!(f, "path is not valid UTF-8"),
            ProtocolError::TooLong(n) => write!(f, "message too long: {n} bytes"),
            ProtocolError::LengthCap(n) => write!(f, "declared length {n} exceeds cap"),
            ProtocolError::Truncated => write!(f, "message truncated"),
            ProtocolError::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            ProtocolError::BadWindowMode(v) => write!(f, "invalid window mode {v}"),
            ProtocolError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        ProtocolError::Io(e)
    }
}

/// Write one framed message (`u32` LE length prefix + payload) to `w`.
pub fn write_message(w: &mut impl Write, req: &OpenRequest) -> Result<(), ProtocolError> {
    let payload = req.encode_payload()?;
    let len = payload.len() as u32; // bounded by MAX_MESSAGE_LEN in encode_payload
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&payload)?;
    w.flush()?;
    Ok(())
}

/// Read one framed message from `r`. Enforces [`MAX_MESSAGE_LEN`] on the declared
/// length before allocating, so a hostile prefix cannot trigger a huge allocation.
pub fn read_message(r: &mut impl Read) -> Result<OpenRequest, ProtocolError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_MESSAGE_LEN {
        return Err(ProtocolError::LengthCap(len));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    OpenRequest::decode_payload(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(req: &OpenRequest) -> OpenRequest {
        let mut buf = Vec::new();
        write_message(&mut buf, req).expect("write");
        let mut cur = Cursor::new(buf);
        read_message(&mut cur).expect("read")
    }

    #[test]
    fn roundtrip_basic_ascii() {
        let req = OpenRequest::new(r"C:\images\test.png");
        assert_eq!(roundtrip(&req), req);
    }

    #[test]
    fn roundtrip_non_ascii_path() {
        // Unicode in the path (spaces, accents, CJK, emoji) must survive intact.
        let req = OpenRequest::new(r"C:\照片\naïve café 🎨\tex.tga");
        assert_eq!(roundtrip(&req), req);
    }

    #[test]
    fn roundtrip_all_flag_variants() {
        for mode in [WindowMode::Default, WindowMode::NewWindow, WindowMode::NewTab] {
            for activate in [true, false] {
                let req = OpenRequest {
                    path: PathBuf::from(r"D:\a.exr"),
                    flags: OpenFlags { window_mode: mode, activate },
                };
                assert_eq!(roundtrip(&req), req);
            }
        }
    }

    #[test]
    fn empty_path_roundtrips() {
        // Degenerate but should not panic; the viewer validates existence separately.
        let req = OpenRequest::new("");
        assert_eq!(roundtrip(&req), req);
    }

    #[test]
    fn declared_length_over_cap_is_rejected_without_allocating() {
        // A hostile length prefix far over the cap must error, not attempt a huge alloc.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_MESSAGE_LEN + 1).to_le_bytes());
        // No payload follows; the cap check must fire before any read of the body.
        let mut cur = Cursor::new(buf);
        match read_message(&mut cur) {
            Err(ProtocolError::LengthCap(n)) => assert_eq!(n, MAX_MESSAGE_LEN + 1),
            other => panic!("expected LengthCap, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_oversized_path() {
        let huge = "a".repeat(MAX_MESSAGE_LEN as usize);
        let req = OpenRequest::new(huge);
        match write_message(&mut Vec::new(), &req) {
            Err(ProtocolError::TooLong(_)) => {}
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn truncated_payload_errors() {
        // Length says 10 but only 2 bytes follow.
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(&[PROTOCOL_VERSION, 0]);
        let mut cur = Cursor::new(buf);
        assert!(matches!(
            read_message(&mut cur),
            Err(ProtocolError::Io(_)) // read_exact hits EOF
        ));
    }

    #[test]
    fn wrong_version_errors() {
        let mut payload = vec![0u8; 4];
        payload[0] = 99; // bad version
        let mut buf = Vec::new();
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);
        let mut cur = Cursor::new(buf);
        assert!(matches!(
            read_message(&mut cur),
            Err(ProtocolError::UnsupportedVersion(99))
        ));
    }
}
