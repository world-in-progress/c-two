use super::*;

#[test]
fn cloned_io_errors_preserve_native_context_and_refusal_markers() {
    fn assert_copy(original: &io::Error) -> io::Error {
        let copy = clone_io_error(original);
        assert_eq!(copy.kind(), original.kind());
        assert_eq!(copy.raw_os_error(), original.raw_os_error());
        assert_eq!(copy.to_string(), original.to_string());
        assert_eq!(
            EndpointIoError::from(&copy),
            EndpointIoError::from(original)
        );
        assert_eq!(
            EndpointBindCause::from(&copy),
            EndpointBindCause::from(original)
        );
        assert_eq!(
            is_peer_identity_mismatch(&copy),
            is_peer_identity_mismatch(original)
        );
        copy
    }

    let native = io::Error::from_raw_os_error(2);
    assert_eq!(assert_copy(&native).raw_os_error(), Some(2));

    let context = io::Error::new(
        io::ErrorKind::PermissionDenied,
        ContextIoError {
            message: "endpoint directory policy context".into(),
            source: native,
        },
    );
    let copy = assert_copy(&context);
    assert_eq!(copy.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(EndpointIoError::from(&copy).raw_os_error, Some(2));
    let inner = copy
        .get_ref()
        .unwrap()
        .downcast_ref::<ContextIoError>()
        .unwrap();
    assert_eq!(inner.source.raw_os_error(), Some(2));

    let identity = peer_identity_mismatch("native peer owner differs");
    assert!(is_peer_identity_mismatch(&assert_copy(&identity)));
    let context = io::Error::new(
        io::ErrorKind::PermissionDenied,
        ContextIoError {
            message: "peer identity context".into(),
            source: identity,
        },
    );
    let copy = assert_copy(&context);
    let inner = copy
        .get_ref()
        .unwrap()
        .downcast_ref::<ContextIoError>()
        .unwrap();
    assert!(is_peer_identity_mismatch(&inner.source));

    let refusal = io::Error::new(
        io::ErrorKind::InvalidData,
        EndpointUnverifiedReason::Symlink,
    );
    let copy = assert_copy(&refusal);
    assert_eq!(
        copy.get_ref()
            .unwrap()
            .downcast_ref::<EndpointUnverifiedReason>(),
        Some(&EndpointUnverifiedReason::Symlink)
    );

    let diagnostic = io::Error::new(io::ErrorKind::PermissionDenied, "ordinary access refusal");
    assert!(!is_peer_identity_mismatch(&assert_copy(&diagnostic)));
}
