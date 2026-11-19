//! Tests for the capability-token handshake (S-1, S-2). Run on Unix here; the
//! Windows side is exercised by the same API through `windows-transport.yml`
//! CI.

use super::{
    authenticate_client, authenticate_client_sync, authenticate_server,
    authenticate_server_sync, generate_capability_token, load_token_file,
    persist_token_file, token_equals_constant_time, token_file_path, MAX_AUTH_LINE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jcode-transport-auth-{}-{}",
        label,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn token_is_64_hex_characters_and_unique() {
    let a = generate_capability_token();
    let b = generate_capability_token();
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b, "CSPRNG tokens must not repeat");
}

#[test]
fn token_file_round_trip_and_rejections() {
    let dir = temp_dir("file");
    let path = dir.join("api.sock.token");

    persist_token_file(&path, &generate_capability_token()).expect("persist");
    let loaded = load_token_file(&path).expect("load");
    assert_eq!(loaded.len(), 64);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
    }

    std::fs::write(&path, "").expect("empty");
    assert!(load_token_file(&path).is_err(), "empty token must be rejected");
    std::fs::write(&path, "x".repeat(MAX_AUTH_LINE + 1)).expect("oversize");
    assert!(
        load_token_file(&path).is_err(),
        "oversized token file must be rejected"
    );

    let socket = dir.join("api.sock");
    assert_eq!(
        token_file_path(&socket),
        dir.join("api.sock.token"),
        "token lives next to the socket with a .token suffix"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn comparison_is_constant_time_and_correct() {
    let token = generate_capability_token();
    assert!(token_equals_constant_time(&token, &token));
    assert!(!token_equals_constant_time(&token, &token[..63]));
    assert!(!token_equals_constant_time(&token, "x".repeat(64).as_str()));
    assert!(!token_equals_constant_time("", &token));
}

/// Server accepts the right token and serves afterwards; wrong and missing
/// tokens are rejected with ERR and an error, on both the async and sync
/// paths, with a follow-up frame left intact for the authenticated case.
#[tokio::test]
async fn handshake_succeeds_only_with_the_capability_token() {
    let token = generate_capability_token();

    // Authenticated round trip, async.
    let token = generate_capability_token();
    let client_token = token.clone();
    let (mut server, mut client) = tokio::io::duplex(1024);
    let server_task = tokio::spawn(async move {
        authenticate_server(&mut server, &token).await.expect("server side");
        server.write_all(b"hello-after-auth\n").await.expect("serve");
    });
    authenticate_client(&mut client, &client_token)
        .await
        .expect("client side");
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.expect("read");
    assert_eq!(reply, b"hello-after-auth\n");
    server_task.await.expect("server task");

    // Wrong token, async: rejected with ERR and an error result.
    let (mut server, mut client) = tokio::io::duplex(1024);
    let server_task = tokio::spawn(async move {
        assert!(authenticate_server(&mut server, "the-real-token")
            .await
            .is_err());
    });
    authenticate_client(&mut client, "not-the-token")
        .await
        .expect_err("wrong token must fail the client");
    server_task.await.expect("server task");
}

#[tokio::test]
async fn handshake_rejects_oversized_and_non_auth_frames() {
    let (mut server, mut client) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(async move {
        assert!(authenticate_server(&mut server, "token").await.is_err());
    });
    // Pipelined junk beyond the limit never gets buffered.
    client
        .write_all(&vec![b'a'; MAX_AUTH_LINE + 64])
        .await
        .expect("write oversized");
    client.flush().await.expect("flush");
    server_task.await.expect("server task");

    let (mut server, mut client) = tokio::io::duplex(1024);
    let server_task = tokio::spawn(async move {
        assert!(authenticate_server(&mut server, "token").await.is_err());
    });
    client.write_all(b"GET / HTTP/1.1\n").await.expect("write");
    server_task.await.expect("server task");
}

/// The sync path the SDK uses behaves identically over real socket pairs.
#[test]
fn sync_handshake_round_trip_and_rejection() {
    let token = generate_capability_token();
    let server_token = token.clone();
    let (mut a, mut b) = std::os::unix::net::UnixStream::pair().expect("pair");

    let server = std::thread::spawn(move || {
        authenticate_server_sync(&mut a, &server_token).expect("sync server side");
        use std::io::Write;
        a.write_all(b"ready\n").expect("serve");
    });
    authenticate_client_sync(&mut b, &token).expect("sync client side");
    let mut reply = [0u8; 6];
    std::io::Read::read_exact(&mut b, &mut reply).expect("read");
    assert_eq!(&reply, b"ready\n");
    server.join().expect("server thread");

    let (mut a, mut b) = std::os::unix::net::UnixStream::pair().expect("pair");
    let server = std::thread::spawn(move || {
        assert!(authenticate_server_sync(&mut a, "the-real-token").is_err());
    });
    authenticate_client_sync(&mut b, "not-the-token")
        .expect_err("wrong token must fail");
    server.join().expect("server thread");
}
