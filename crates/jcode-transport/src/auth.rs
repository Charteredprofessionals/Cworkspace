//! Capability-token authentication for local IPC connections (S-1, S-2).
//!
//! The transport's job is bytes; this module gives callers the missing
//! *access control*. The finalization audit found the desktop daemon accepted
//! every local client and broadcast approval IDs to all of them: nothing
//! authenticated the peer. The rewritten codebase fixed the Unix side with a
//! 0600 socket mode, but the Windows named pipe has no file mode — the default
//! pipe DACL comes from the creating process and is not an access guarantee
//! (the bridge even carried a comment claiming an ACL "the transport applies
//! when publishing the pipe"; no such code existed). This handshake is the
//! access control that works identically on both platforms:
//!
//! 1. The server generates a 256-bit capability token from the OS CSPRNG and
//!    persists it next to the socket/pipe with restrictive file permissions.
//!    Only processes that can read that file — the owning user's processes —
//!    can ever authenticate.
//! 2. The client's first frame is `AUTH <token>\n`. The server compares it in
//!    constant time and answers `OK\n`, or answers `ERR\n` and drops the
//!    connection. No API frame is served before authentication succeeds.
//!
//! Bounded reads everywhere (S-7): an unauthenticated client can never push
//! more than [`MAX_AUTH_LINE`] bytes at the server. The handshake reads one
//! byte at a time instead of buffering, so it never consumes frames a client
//! pipelines after `AUTH` before receiving `OK`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Longest accepted `AUTH` line. A real token is 64 hex chars; anything
/// longer is junk or an attack and is rejected before buffering it.
pub const MAX_AUTH_LINE: usize = 4096;

/// Path of the capability token file for a socket/pipe at `socket`:
/// the socket path plus a `.token` suffix.
pub fn token_file_path(socket: &std::path::Path) -> std::path::PathBuf {
    let mut path = socket.as_os_str().to_owned();
    path.push(".token");
    std::path::PathBuf::from(path)
}

/// Generate a 256-bit capability token from the operating system CSPRNG,
/// hex-encoded (64 characters).
pub fn generate_capability_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .expect("OS CSPRNG unavailable: cannot mint capability tokens");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Persist a capability token next to its socket. On Unix the file is created
/// with 0600; on Windows it inherits the creating user's profile ACLs, which
/// is the same scope the handshake enforces anyway.
pub fn persist_token_file(path: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(token.as_bytes())?;
    file.sync_all()
}

/// Load a capability token previously written by [`persist_token_file`].
/// Refuses oversized files: the token is 64 bytes, so anything bigger is not
/// a token file.
pub fn load_token_file(path: &std::path::Path) -> std::io::Result<String> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_AUTH_LINE as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "capability token file exceeds the maximum token size",
        ));
    }
    let token = std::fs::read_to_string(path)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "capability token file is empty",
        ));
    }
    Ok(token.to_owned())
}

/// Byte-length-independent constant-time comparison, so a failed handshake
/// does not leak token content through timing.
fn token_equals_constant_time(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u32;
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        diff |= (left ^ right) as u32;
    }
    diff == 0
}

/// Read one newline-terminated line without buffering past it, refusing
/// longer than `max` bytes. Never consumes bytes after the newline, so a
/// client that pipelines frames after `AUTH` keeps them intact.
fn read_line_bounded_sync<S: std::io::Read>(stream: &mut S, max: usize) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before the AUTH frame completed",
            ));
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        line.push(byte[0]);
        if line.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("AUTH frame exceeds the {max} byte limit"),
            ));
        }
    }
}

async fn read_line_bounded_async<S: AsyncRead + Unpin>(
    stream: &mut S,
    max: usize,
) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before the AUTH frame completed",
            ));
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        line.push(byte[0]);
        if line.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("AUTH frame exceeds the {max} byte limit"),
            ));
        }
    }
}

/// Server side of the handshake (async, for the bridge accept loop). Reads
/// the client's first line, requires `AUTH <token>`, answers `OK\n` on
/// success. Any deviation answers `ERR\n` and fails.
pub async fn authenticate_server<S>(stream: &mut S, expected_token: &str) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let line = read_line_bounded_async(stream, MAX_AUTH_LINE).await?;
    let token = line.strip_prefix("AUTH ").map(str::trim_end).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "missing AUTH frame")
    })?;
    if !token_equals_constant_time(token, expected_token) {
        let _ = stream.write_all(b"ERR\n").await;
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "capability token rejected",
        ));
    }
    stream.write_all(b"OK\n").await
}

/// Client side of the handshake (async). Sends `AUTH <token>\n` and waits for
/// the server's `OK\n`.
pub async fn authenticate_client<S>(stream: &mut S, token: &str) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(format!("AUTH {token}\n").as_bytes())
        .await?;
    stream.flush().await?;
    let mut reply = [0u8; 3];
    stream.read_exact(&mut reply).await?;
    if &reply != b"OK\n" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "server rejected the capability token",
        ));
    }
    Ok(())
}

/// Server side of the handshake (sync, for blocking callers such as the SDK).
pub fn authenticate_server_sync<S>(stream: &mut S, expected_token: &str) -> std::io::Result<()>
where
    S: std::io::Read + std::io::Write,
{
    use std::io::Write as _;
    let line = read_line_bounded_sync(stream, MAX_AUTH_LINE)?;
    let token = line.strip_prefix("AUTH ").map(str::trim_end).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "missing AUTH frame")
    })?;
    if !token_equals_constant_time(token, expected_token) {
        let _ = stream.write_all(b"ERR\n");
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "capability token rejected",
        ));
    }
    stream.write_all(b"OK\n")
}

/// Client side of the handshake (sync). Sends `AUTH <token>\n`, waits for
/// `OK\n`.
pub fn authenticate_client_sync<S>(stream: &mut S, token: &str) -> std::io::Result<()>
where
    S: std::io::Read + std::io::Write,
{
    use std::io::Write;
    stream.write_all(format!("AUTH {token}\n").as_bytes())?;
    stream.flush()?;
    let mut reply = [0u8; 3];
    stream.read_exact(&mut reply)?;
    if &reply != b"OK\n" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "server rejected the capability token",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod auth_tests;
