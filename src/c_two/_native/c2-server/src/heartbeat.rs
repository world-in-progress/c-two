//! Heartbeat probing for a single connection.
//!
//! Mirrors the Python `run_heartbeat()` from
//! `c_two.transport.server.heartbeat`.  Periodically checks idle time
//! and sends a PING signal frame when the connection is quiet.

use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use tracing::warn;

use crate::config::IpcConfig;
use crate::connection::Connection;

/// Outcome of the heartbeat task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatResult {
    /// Client exceeded `heartbeat_timeout` without any activity.
    TimedOut,
    /// Failed to write the PING frame (broken pipe / closed socket).
    WriteError,
    /// Heartbeat was disabled (`heartbeat_interval <= 0`).
    Disabled,
}

/// Build a PING signal frame.
///
/// Frame layout (17 bytes):
/// ```text
/// [4B total_len LE][8B request_id LE][4B flags LE][1B signal_type]
/// ```
/// - `total_len` = 13 (8 + 4 + 1)
/// - `request_id` = 0 (signals have no request context)
/// - `flags` = `FLAG_SIGNAL` (0x800)
/// - `signal_type` = `MsgType::Ping` (0x01)
fn build_ping_frame() -> Vec<u8> {
    use c2_wire::flags::FLAG_SIGNAL;
    use c2_wire::msg_type::MsgType;

    c2_wire::frame::encode_frame(0, FLAG_SIGNAL, &[MsgType::Ping.as_byte()])
}

/// Run heartbeat probing for a single connection.
///
/// The task checks idle time every `heartbeat_interval / 2` seconds.
///
/// - If idle ≥ `heartbeat_timeout` → returns [`HeartbeatResult::TimedOut`].
/// - If idle ≥ `heartbeat_interval` → sends a PING frame.
/// - If `heartbeat_interval` ≤ 0 → heartbeat is disabled; waits forever.
pub async fn run_heartbeat(
    conn: Arc<Connection>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    config: &IpcConfig,
) -> HeartbeatResult {
    if config.heartbeat_interval <= 0.0 {
        // Heartbeat disabled — park forever.
        std::future::pending::<()>().await;
        return HeartbeatResult::Disabled;
    }

    let check_interval = Duration::from_secs_f64(config.heartbeat_interval / 2.0);
    let mut ticker = interval(check_interval);
    // Skip the immediate first tick (fires instantly).
    ticker.tick().await;

    let ping_frame = build_ping_frame();

    loop {
        ticker.tick().await;
        let idle = conn.idle_seconds();

        if idle >= config.heartbeat_timeout {
            warn!(
                conn_id = conn.conn_id(),
                idle_secs = format!("{idle:.1}"),
                "heartbeat timeout — closing connection"
            );
            return HeartbeatResult::TimedOut;
        }

        if idle >= config.heartbeat_interval {
            let mut w = writer.lock().await;
            if w.write_all(&ping_frame).await.is_err() {
                warn!(conn_id = conn.conn_id(), "heartbeat write failed");
                return HeartbeatResult::WriteError;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_frame_structure() {
        let frame = build_ping_frame();

        // 4 (total_len) + 8 (rid) + 4 (flags) + 1 (signal) = 17
        assert_eq!(frame.len(), 17);

        let total_len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        // total_len = 12 (rid + flags) + 1 (payload) = 13
        assert_eq!(total_len, 13);

        let request_id = u64::from_le_bytes(frame[4..12].try_into().unwrap());
        assert_eq!(request_id, 0);

        let flags = u32::from_le_bytes(frame[12..16].try_into().unwrap());
        assert_eq!(flags, c2_wire::flags::FLAG_SIGNAL);

        assert_eq!(frame[16], c2_wire::msg_type::MsgType::Ping.as_byte());
    }

    #[tokio::test]
    async fn disabled_heartbeat() {
        let conn = Arc::new(Connection::new(1));
        let config = IpcConfig {
            heartbeat_interval: 0.0,
            ..IpcConfig::default()
        };

        // The run_heartbeat call with interval=0 should be disabled.
        // We can't easily test "parks forever", so we race it against a timeout.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        // We need a real UDS pair for the writer; use a dummy.
        let (sock_a, _sock_b) = tokio::net::UnixStream::pair().unwrap();
        let (_reader, write_half) = sock_a.into_split();
        let writer = Arc::new(Mutex::new(write_half));

        let conn2 = Arc::clone(&conn);
        let handle = tokio::spawn(async move {
            tokio::select! {
                result = run_heartbeat(conn2, writer, &config) => {
                    assert_eq!(result, HeartbeatResult::Disabled);
                }
                _ = async { rx.await.ok() } => {
                    // Test timeout — heartbeat correctly parked.
                }
            }
        });

        // Let it park for a bit, then cancel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(());
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn heartbeat_sends_ping_on_idle() {
        use tokio::io::AsyncReadExt;

        let conn = Arc::new(Connection::new(1));
        let config = IpcConfig {
            heartbeat_interval: 0.1,  // 100ms
            heartbeat_timeout: 10.0,  // far away
            ..IpcConfig::default()
        };

        let (sock_a, sock_b) = tokio::net::UnixStream::pair().unwrap();
        let (_read_a, write_half) = sock_a.into_split();
        let (mut read_b, _write_b) = sock_b.into_split();
        let writer = Arc::new(Mutex::new(write_half));

        let conn2 = Arc::clone(&conn);
        let hb = tokio::spawn(async move {
            run_heartbeat(conn2, writer, &config).await
        });

        // Wait enough time for the heartbeat to fire and send a PING.
        let mut buf = [0u8; 17];
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            read_b.read_exact(&mut buf),
        )
        .await;

        assert!(result.is_ok(), "should receive PING frame");
        let read_result = result.unwrap();
        assert!(read_result.is_ok(), "read should succeed");

        // Validate it's a valid PING frame.
        let flags = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        assert_eq!(flags, c2_wire::flags::FLAG_SIGNAL);
        assert_eq!(buf[16], 0x01); // MsgType::Ping

        hb.abort();
    }

    #[tokio::test]
    async fn heartbeat_timeout_returns() {
        let conn = Arc::new(Connection::new(1));
        let config = IpcConfig {
            heartbeat_interval: 0.05,
            heartbeat_timeout: 0.10,
            ..IpcConfig::default()
        };

        let (sock_a, _sock_b) = tokio::net::UnixStream::pair().unwrap();
        let (_read_a, write_half) = sock_a.into_split();
        let writer = Arc::new(Mutex::new(write_half));

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run_heartbeat(conn, writer, &config),
        )
        .await;

        assert!(result.is_ok(), "heartbeat should return before outer timeout");
        assert_eq!(result.unwrap(), HeartbeatResult::TimedOut);
    }
}
