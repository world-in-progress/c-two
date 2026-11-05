use super::*;

#[test]
fn read_and_write_share_peer_closed_classification_but_only_reads_prove_end() {
    let mut errors = vec![io::Error::from(io::ErrorKind::UnexpectedEof)];
    #[cfg(unix)]
    errors.extend([
        io::Error::from_raw_os_error(libc::EPIPE),
        io::Error::from_raw_os_error(libc::ECONNRESET),
    ]);
    #[cfg(windows)]
    errors.extend([
        io::Error::from_raw_os_error(109),
        io::Error::from_raw_os_error(232),
    ]);
    for error in errors {
        let expected = if error.kind() == io::ErrorKind::ConnectionReset {
            ConnectionEndReason::ConnectionReset
        } else {
            ConnectionEndReason::PeerEof
        };
        let writer = ConnectionPublisher::new(ConnectionSide::Server);
        writer.close_io("flush", &error);
        let write_end = writer.observation().snapshot().unwrap();
        assert_eq!(write_end.reason(), expected, "{error:?}");
        assert!(
            !write_end.is_peer_end(),
            "write failure never authorizes reclaim"
        );
        assert_eq!(write_end.os_error(), error.raw_os_error());

        let reader = ConnectionPublisher::new(ConnectionSide::Server);
        reader.close_read_error("read_length", &error);
        let read_end = reader.observation().snapshot().unwrap();
        assert_eq!(read_end.reason(), expected, "{error:?}");
        assert!(read_end.is_peer_end(), "native read end authorizes reclaim");
        assert_eq!(read_end.os_error(), error.raw_os_error());
    }
}

#[test]
fn local_failures_are_not_peer_closed() {
    let mut errors = vec![
        io::Error::from(io::ErrorKind::PermissionDenied),
        io::Error::from(io::ErrorKind::TimedOut),
        io::Error::from(io::ErrorKind::ConnectionAborted),
        io::Error::from(io::ErrorKind::WriteZero),
    ];
    #[cfg(windows)]
    errors.push(io::Error::from_raw_os_error(233));
    for error in errors.drain(..) {
        for read in [false, true] {
            let publisher = ConnectionPublisher::new(ConnectionSide::Server);
            if read {
                publisher.close_read_error("read_length", &error);
            } else {
                publisher.close_io("flush", &error);
            }
            let closed = publisher.observation().snapshot().unwrap();
            assert_eq!(closed.reason(), ConnectionEndReason::IoError, "{error:?}");
            assert!(!closed.is_peer_end());
        }
    }
}
