//! Focused codec tests for the endpoint credential JSON form.
//!
//! Every negative case is a complete document a permissive parser could
//! plausibly accept; each must be rejected instead.
//!
//! The module is deliberately not gated as a whole: the strict-parse rules for
//! fields, size, and schema version are platform-neutral and must run on both
//! Unix and Windows. Only the identity-carrying cases are Unix-specific, and
//! Windows additionally proves that a kernel-managed credential round-trips
//! and still rejects Unix identity fields and managed-v2 records.

use super::*;

/// The protocol type is named directly because this test module is a sibling of
/// the codec module, not a child, so `super::*` is the crate root.
#[cfg(any(unix, windows))]
use c2_config::LocalEndpointProtocol;

/// The platform name this build must accept, derived the same way the codec
/// derives it. Keeping it local avoids widening the codec's API for a test.
fn host_platform() -> &'static str {
    if cfg!(windows) { "windows" } else { "unix" }
}

/// Binds a real managed listener so credentials under test are genuine native
/// values, never hand-built structs.
#[cfg(unix)]
fn managed() -> (LocalListener, EndpointCredential) {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let endpoint = LocalEndpoint::from_address_with_protocol(
        &format!("ipc://codec-{}", &unique[..16]),
        c2_config::LocalEndpointProtocol::ManagedV2,
    )
    .unwrap();
    let listener = LocalListener::bind(&endpoint).expect("bind managed listener");
    let credential = listener.credential();
    (listener, credential)
}

#[cfg(unix)]
fn legacy() -> (LocalListener, EndpointCredential) {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://codec-{:016}", &unique[..16])).unwrap();
    let listener = LocalListener::bind(&endpoint).expect("bind legacy listener");
    let credential = listener.credential();
    (listener, credential)
}

/// The kernel-managed credential a Windows listener hands out. The endpoint is
/// real, so the value under test is native rather than hand-assembled.
#[cfg(windows)]
fn kernel_managed() -> EndpointCredential {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let endpoint =
        LocalEndpoint::from_address_with_protocol(
            &format!("ipc://codec-{:016}", &unique[..16]),
            LocalEndpointProtocol::LegacyV1,
        )
        .unwrap();
    EndpointCredential::kernel_managed(endpoint)
}

fn decoded(json: &str) -> Result<EndpointCredential, EndpointCredentialError> {
    EndpointCredential::from_json(json)
}

#[cfg(unix)]
async fn inspect_managed_when_gate_available(endpoint: &LocalEndpoint) -> EndpointCredential {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match inspect_endpoint(endpoint) {
            EndpointInspection::Present(credential) => return credential,
            EndpointInspection::IoError(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(std::time::Instant::now() < deadline, "namespace gate stayed busy");
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            other => panic!("a live managed listener must inspect as present, got {other:?}"),
        }
    }
}

fn reject(json: &str) -> EndpointCredentialErrorKind {
    decoded(json).expect_err("document must be rejected").kind()
}

/// Rewrites the document into an object, applies `edit`, and re-serializes.
/// Field order and comma placement in the source text never matter.
#[cfg(unix)]
fn mutate(
    json: &str,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> String {
    let mut value: serde_json::Value = serde_json::from_str(json).unwrap();
    edit(value.as_object_mut().unwrap());
    serde_json::to_string(&value).unwrap()
}

fn assert_rejected(json: &str, expected: &[EndpointCredentialErrorKind]) {
    let kind = reject(json);
    assert!(
        expected.contains(&kind),
        "unexpected rejection kind: {kind:?} for {json}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn managed_credential_round_trips_without_a_path() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    assert!(!json.contains("/tmp"), "document must not carry an OS path");
    assert!(credential.incarnation().is_some());

    let round_tripped = decoded(&json).unwrap();
    assert_eq!(round_tripped, credential);
    // The OS name is re-derived, never read back from the document.
    assert_eq!(
        round_tripped.endpoint().os_name(),
        credential.endpoint().os_name()
    );
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_v1_credential_round_trips_without_an_incarnation() {
    let (listener, credential) = legacy();
    assert_eq!(credential.incarnation(), None);
    let json = credential.to_json().unwrap();
    assert!(!json.contains("incarnation"));

    let round_tripped = decoded(&json).unwrap();
    assert_eq!(round_tripped, credential);
    assert_eq!(round_tripped.incarnation(), None);
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_unknown_fields_unknown_schema_and_oversize() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    assert_rejected(
        &mutate(&json, |object| {
            object.insert("socketPath".into(), "/tmp/evil.sock".into());
        }),
        &[EndpointCredentialErrorKind::MalformedJson],
    );
    assert_rejected(
        &mutate(&json, |object| {
            object.insert("schemaVersion".into(), 3.into());
        }),
        &[EndpointCredentialErrorKind::UnsupportedSchemaVersion],
    );
    let padding = "a".repeat(ENDPOINT_CREDENTIAL_MAX_BYTES);
    assert_rejected(
        &format!("{{\"schemaVersion\":2,\"address\":\"{padding}\"}}"),
        &[EndpointCredentialErrorKind::TooLarge],
    );
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_missing_required_fields() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    for field in ["address", "protocol", "platform", "incarnation", "inode"] {
        assert_rejected(
            &mutate(&json, |object| {
                object.remove(field);
            }),
            &[
                EndpointCredentialErrorKind::MissingField,
                EndpointCredentialErrorKind::MalformedJson,
                // Dropping the incarnation is a specific, equally strict error.
                EndpointCredentialErrorKind::IncarnationRequired,
            ],
        );
    }
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn v2_requires_an_incarnation_and_legacy_records_are_never_upgraded() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    assert_rejected(
        &mutate(&json, |object| {
            object.remove("incarnation");
        }),
        &[EndpointCredentialErrorKind::IncarnationRequired],
    );
    drop(listener);

    // A legacy endpoint claiming v2 is not promoted: it has no incarnation,
    // so it fails rather than becoming a UUID credential.
    let (listener, credential) = legacy();
    let json = credential.to_json().unwrap();
    assert_rejected(
        &mutate(&json, |object| {
            object.insert("schemaVersion".into(), 2.into());
        }),
        &[EndpointCredentialErrorKind::IncarnationRequired],
    );
    // A v1 record must not smuggle an incarnation in either.
    assert_rejected(
        &mutate(&json, |object| {
            object.insert(
                "incarnation".into(),
                "00112233445566778899aabbccddeeff".into(),
            );
        }),
        &[EndpointCredentialErrorKind::InvalidValue],
    );
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_checked_integer_and_identity_violations() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    for (field, value) in [
        // Beyond u64, wrongly typed, negative, and fractional integers must
        // all fail rather than coerce.
        ("device", serde_json::json!("18446744073709551616")),
        ("inode", serde_json::json!("1")),
        ("changedSecs", serde_json::json!(-1.5)),
        ("changedNanos", serde_json::json!(0.5)),
    ] {
        assert_rejected(
            &mutate(&json, |object| {
                object.insert(field.into(), value.clone());
            }),
            &[
                EndpointCredentialErrorKind::InvalidValue,
                EndpointCredentialErrorKind::MalformedJson,
            ],
        );
    }
    // A short or non-hex incarnation is rejected, not padded or coerced.
    for bad in ["zz", "00ff", ""] {
        assert_rejected(
            &mutate(&json, |object| {
                object.insert("incarnation".into(), bad.into());
            }),
            &[EndpointCredentialErrorKind::InvalidValue],
        );
    }
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_address_protocol_and_platform_mismatches() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    // The OS path is never authority: path-like and non-local addresses fail
    // the config derivation instead of being accepted as a filesystem path.
    for address in ["ipc://x/y", "ipc://..", "tcp://host", "/tmp/whatever.sock"] {
        assert_rejected(
            &mutate(&json, |object| {
                object.insert("address".into(), address.into());
            }),
            &[
                EndpointCredentialErrorKind::InvalidValue,
                EndpointCredentialErrorKind::MalformedJson,
            ],
        );
    }
    for protocol in ["managed-v3", "legacy"] {
        assert_rejected(
            &mutate(&json, |object| {
                object.insert("protocol".into(), protocol.into());
            }),
            &[EndpointCredentialErrorKind::InvalidValue],
        );
    }
    assert_rejected(
        &mutate(&json, |object| {
            object.insert("platform".into(), "windows".into());
        }),
        &[EndpointCredentialErrorKind::UnsupportedPlatform],
    );
    // A managed record cannot name the legacy protocol under schema v2.
    assert_rejected(
        &mutate(&json, |object| {
            object.insert("protocol".into(), "legacy-v1".into());
        }),
        &[
            EndpointCredentialErrorKind::InvalidValue,
            EndpointCredentialErrorKind::IncarnationRequired,
        ],
    );
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_trailing_garbage_and_non_object_documents() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    for document in ["[]", "null", "42", "{}"] {
        assert!(
            decoded(document).is_err(),
            "document {document} must be rejected"
        );
    }
    assert_rejected(
        &format!("{json}{{}}"),
        &[EndpointCredentialErrorKind::MalformedJson],
    );
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn decoding_does_not_create_filesystem_state() {
    let (listener, credential) = managed();
    // Decode a syntactically valid credential for an unbound, unique target.
    // Other parallel listeners may change the shared namespace, so observe
    // only this target's native-derived backing names, never a global count.
    let endpoint = LocalEndpoint::from_address_with_protocol(
        &format!("ipc://codec-unbound-{}", uuid::Uuid::new_v4().simple()),
        LocalEndpointProtocol::ManagedV2,
    )
    .unwrap();
    let mut document: serde_json::Value =
        serde_json::from_str(&credential.to_json().unwrap()).unwrap();
    document["address"] = endpoint.address().into();
    let json = document.to_string();
    let socket = std::path::Path::new(endpoint.os_name());
    let stem = socket.file_stem().unwrap().to_str().unwrap();
    let root = socket
        .parent()
        .unwrap()
        .to_path_buf();
    let target_entries = || {
        std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| {
                let name = entry.unwrap().file_name();
                name.to_str().unwrap().starts_with(stem).then_some(name)
            })
            .collect::<Vec<_>>()
    };
    assert!(target_entries().is_empty());
    let round_tripped = decoded(&json).unwrap();
    assert_eq!(round_tripped.endpoint(), &endpoint);
    assert!(
        target_entries().is_empty(),
        "decoding must not create target socket, locks, or markers"
    );
    drop(round_tripped);
    drop(listener);
}

#[cfg(unix)]
#[tokio::test]
async fn a_decoded_managed_credential_reaps_through_the_native_gate() {
    let (listener, credential) = managed();
    let json = credential.to_json().unwrap();
    let decoded = decoded(&json).unwrap();

    // Inspecting the live endpoint must reproduce an equivalent credential,
    // proving the file round-trip preserves exactly the native identity.
    // Only classified namespace contention may delay this observation.
    let inspected = inspect_managed_when_gate_available(decoded.endpoint()).await;
    assert_eq!(inspected.to_json().unwrap(), json);

    // The listener is dropped without an explicit close, leaving the slot for
    // the reaper; the decoded credential is still the proof for that object.
    drop(listener);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let result = loop {
        match reap_endpoint(decoded.endpoint(), &decoded) {
            // A gate held by another process is not a cleanup decision, so
            // retry rather than treat contention as a verdict.
            EndpointReapResult::Busy => {
                assert!(std::time::Instant::now() < deadline, "retired target stayed busy");
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            result => break result,
        }
    };
    assert!(
        matches!(
            result,
            EndpointReapResult::Reaped | EndpointReapResult::AlreadyAbsent
        ),
        "decoded credential must reap its own object, got {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_credential_for_another_endpoint_is_stale_not_authority() {
    let (listener, credential) = managed();
    let decoded = decoded(&credential.to_json().unwrap()).unwrap();
    let (other_listener, other) = managed();

    // Reaping a different live endpoint with this credential must not remove
    // the other listener's socket.
    let result = reap_endpoint(other.endpoint(), &decoded);
    assert!(
        matches!(result, EndpointReapResult::StaleTarget),
        "a foreign credential must be stale, got {result:?}"
    );
    let inspected = inspect_managed_when_gate_available(other.endpoint()).await;
    assert_eq!(inspected.to_json().unwrap(), other.to_json().unwrap());
    // The other listener still closes its own object cleanly.
    let _ = other_listener.close();
    drop(listener);
}

// --- Platform-neutral strict-parse rules -------------------------------------
//
// These cases never touch a listener, so they run on Unix and Windows alike.
// They cover the rules a permissive Windows parser would otherwise skip: field
// set, size cap, schema version, and address-derived endpoint authority.

#[test]
fn rejects_structurally_invalid_documents_on_every_platform() {
    for document in ["[]", "null", "42", "{}", "not json", ""] {
        assert!(
            decoded(document).is_err(),
            "document {document} must be rejected"
        );
    }
}

#[test]
fn enforces_the_size_cap_before_parsing() {
    let padding = "a".repeat(ENDPOINT_CREDENTIAL_MAX_BYTES);
    assert_rejected(
        &format!("{{\"schemaVersion\":1,\"address\":\"{padding}\"}}"),
        &[EndpointCredentialErrorKind::TooLarge],
    );
}

#[test]
fn rejects_unknown_fields_on_every_platform() {
    let document = format!(
        r#"{{"schemaVersion":1,"address":"ipc://codec-neutral","protocol":"legacy-v1","platform":"{}","extraField":1}}"#,
        host_platform()
    );
    assert_rejected(&document, &[EndpointCredentialErrorKind::MalformedJson]);
}

#[test]
fn rejects_an_unsupported_schema_version_on_every_platform() {
    let document = format!(
        r#"{{"schemaVersion":9,"address":"ipc://codec-neutral","protocol":"legacy-v1","platform":"{}"}}"#,
        host_platform()
    );
    assert_rejected(
        &document,
        &[EndpointCredentialErrorKind::UnsupportedSchemaVersion],
    );
}

#[test]
fn rejects_a_non_local_address_on_every_platform() {
    for address in ["tcp://host", "ipc://x/y", "ipc://..", "/tmp/whatever.sock"] {
        let document = format!(
            r#"{{"schemaVersion":1,"address":"{address}","protocol":"legacy-v1","platform":"{}"}}"#,
            host_platform()
        );
        assert_rejected(
            &document,
            &[
                EndpointCredentialErrorKind::InvalidValue,
                EndpointCredentialErrorKind::MalformedJson,
            ],
        );
    }
}

#[test]
fn rejects_an_unknown_protocol_on_every_platform() {
    let document = format!(
        r#"{{"schemaVersion":1,"address":"ipc://codec-neutral","protocol":"legacy","platform":"{}"}}"#,
        host_platform()
    );
    assert_rejected(&document, &[EndpointCredentialErrorKind::InvalidValue]);
}

// --- Windows kernel-managed credentials --------------------------------------

/// A Windows credential is metadata for a kernel-owned namespace: it
/// round-trips through the one codec and never gains Unix identity fields or a
/// UUID incarnation.
#[cfg(windows)]
#[test]
fn windows_kernel_managed_credential_round_trips_as_v1() {
    let credential = kernel_managed();
    let json = credential.to_json().unwrap();
    assert!(
        json.contains(r#""schemaVersion":1"#),
        "a kernel-managed credential is v1: {json}"
    );
    assert!(!json.contains("incarnation"), "no incarnation on Windows");
    assert!(!json.contains("inode"), "no inode on Windows");
    assert!(
        !json.contains("\\\\.\\pipe"),
        "the OS pipe name is never recorded: {json}"
    );

    let round_tripped = decoded(&json).unwrap();
    assert_eq!(round_tripped, credential);
    // The OS name is re-derived from the address, never read back from a file.
    assert_eq!(
        round_tripped.endpoint().os_name(),
        credential.endpoint().os_name()
    );
}

/// `badplatform`: a document naming a platform this build is not must be
/// rejected rather than reinterpreted as a local credential.
#[cfg(windows)]
#[test]
fn windows_rejects_a_unix_platform_document() {
    let document =
        r#"{"schemaVersion":1,"address":"ipc://codec-badplatform","protocol":"legacy-v1","platform":"unix"}"#;
    assert_rejected(&document, &[EndpointCredentialErrorKind::UnsupportedPlatform]);
}

#[cfg(unix)]
#[test]
fn unix_rejects_a_windows_platform_document() {
    let document =
        r#"{"schemaVersion":1,"address":"ipc://codec-badplatform","protocol":"legacy-v1","platform":"windows"}"#;
    assert_rejected(&document, &[EndpointCredentialErrorKind::UnsupportedPlatform]);
}

/// `managedunsupported`: a managed-v2 document needs an incarnation and a
/// managed listener, neither of which a Windows kernel pipe can provide, so it
/// is rejected instead of being downgraded into a collectable credential.
#[cfg(windows)]
#[test]
fn windows_rejects_managed_v2_documents() {
    let with_incarnation = r#"{"schemaVersion":2,"address":"ipc://codec-managedunsupported","protocol":"managed-v2","platform":"windows","incarnation":"00112233445566778899aabbccddeeff"}"#;
    assert_rejected(
        with_incarnation,
        &[EndpointCredentialErrorKind::IncarnationRequired],
    );
    // A Unix identity field is not a Windows pipe property either.
    let with_identity = r#"{"schemaVersion":1,"address":"ipc://codec-managedunsupported","protocol":"legacy-v1","platform":"windows","device":1,"inode":2}"#;
    assert_rejected(with_identity, &[EndpointCredentialErrorKind::InvalidValue]);
}
