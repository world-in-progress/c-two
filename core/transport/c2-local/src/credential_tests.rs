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

fn host_platform() -> &'static str {
    if cfg!(windows) { "windows" } else { "unix" }
}
#[cfg(unix)]
fn managed() -> (LocalListener, EndpointCredential) {
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://codec-{}", uuid::Uuid::new_v4().simple()))
            .unwrap();
    let listener = LocalListener::bind(&endpoint).expect("bind native listener");
    let credential = listener.credential();
    (listener, credential)
}

/// The kernel-managed credential a Windows listener hands out. The endpoint is
/// real, so the value under test is native rather than hand-assembled.
#[cfg(windows)]
fn kernel_managed() -> EndpointCredential {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let endpoint =
        LocalEndpoint::from_address(&format!("ipc://codec-{:016}", &unique[..16])).unwrap();
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
            EndpointInspection::IoError(error)
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "namespace gate stayed busy"
                );
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
    assert_ne!(credential.incarnation(), [0; 16]);

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
            object.insert("schemaVersion".into(), 4.into());
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
    for schema_version in [1, 2] {
        assert_rejected(
            &mutate(&json, |object| {
                object.insert("schemaVersion".into(), schema_version.into());
                object.insert("protocol".into(), "legacy-v1".into());
                object.remove("incarnation");
            }),
            &[EndpointCredentialErrorKind::InvalidValue],
        );
    }
    assert_rejected(
        &mutate(&json, |object| {
            object.insert("schemaVersion".into(), 1.into());
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
    let endpoint = LocalEndpoint::from_address(&format!(
        "ipc://codec-unbound-{}",
        uuid::Uuid::new_v4().simple()
    ))
    .unwrap();
    let mut document: serde_json::Value =
        serde_json::from_str(&credential.to_json().unwrap()).unwrap();
    document["address"] = endpoint.address().into();
    let json = document.to_string();
    let socket = std::path::Path::new(endpoint.os_name());
    let stem = socket.file_stem().unwrap().to_str().unwrap();
    let root = socket.parent().unwrap().to_path_buf();
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
                assert!(
                    std::time::Instant::now() < deadline,
                    "retired target stayed busy"
                );
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

// These values are descriptions for codec tests, deliberately constructed
// without a bind. Only the native maintenance checks can authorize a reap.
#[cfg(unix)]
fn described_credential(context: &LocalEndpointContext) -> EndpointCredential {
    EndpointCredential::unix_managed(
        context.endpoint("ipc://codec-context").unwrap(),
        UnixSocketIdentity {
            device: 11,
            inode: 22,
            changed_secs: 33,
            changed_nanos: 44,
        },
        [0x5a; 16],
    )
}

#[cfg(unix)]
#[test]
fn context_codec_default_remains_v2_and_custom_root_round_trips_v3() {
    let default = described_credential(&LocalEndpointContext::default_for_platform().unwrap());
    let json = default.to_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["schemaVersion"], 2);
    assert!(value.get("unixRoot").is_none());
    assert!(value.get("namespaceId").is_none());
    assert_eq!(decoded(&json).unwrap(), default);

    // Nonexistent roots are valid descriptions. Decode must not create them.
    let root = format!(
        "/tmp/c2c-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    assert!(!std::path::Path::new(&root).exists());
    let context = LocalEndpointContext::with_unix_root(std::path::Path::new(&root)).unwrap();
    let custom = described_credential(&context);
    let json = custom.to_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["schemaVersion"], 3);
    assert_eq!(value["unixRoot"], root);
    assert_eq!(value["namespaceId"], context.namespace_id());
    assert_eq!(value["protocol"], "managed-v2");
    assert_eq!(decoded(&json).unwrap(), custom);
    assert!(!std::path::Path::new(&root).exists());
}

#[cfg(unix)]
#[test]
fn context_codec_v3_strictly_binds_root_and_namespace() {
    let context =
        LocalEndpointContext::with_unix_root(std::path::Path::new("/tmp/c2-codec")).unwrap();
    let json = described_credential(&context).to_json().unwrap();
    for field in ["unixRoot", "namespaceId", "device", "inode", "incarnation"] {
        assert!(
            decoded(&mutate(&json, |object| {
                object.remove(field);
            }))
            .is_err()
        );
        assert!(
            decoded(&mutate(&json, |object| {
                object.insert(field.into(), serde_json::Value::Null);
            }))
            .is_err()
        );
    }
    for root in ["relative", "", "/tmp/../elsewhere", "/tmp/a\0b", "/tmp"] {
        assert_eq!(
            decoded(&mutate(&json, |object| {
                object.insert("unixRoot".into(), root.into());
            }))
            .unwrap_err()
            .field(),
            Some("unixRoot"),
        );
    }
    assert_eq!(
        decoded(&mutate(&json, |object| {
            object.insert("unixRoot".into(), "/tmp/c2-other".into());
        }))
        .unwrap_err()
        .field(),
        Some("namespaceId"),
    );
    assert_eq!(
        decoded(&mutate(&json, |object| {
            object.insert("namespaceId".into(), "forged".into());
        }))
        .unwrap_err()
        .field(),
        Some("namespaceId"),
    );
    // Root normalization is lexical and belongs to c2-config. No canonicalize
    // or filesystem alias rewriting participates in this comparison.
    let normalized = decoded(&mutate(&json, |object| {
        object.insert("unixRoot".into(), "/tmp//c2-codec/".into());
    }))
    .unwrap();
    assert_eq!(normalized.endpoint().context(), &context);
    assert!(
        decoded(&mutate(&json, |object| {
            object.insert("socketPath".into(), "/tmp/arbitrary".into());
        }))
        .is_err()
    );
    assert!(
        decoded(&mutate(&json, |object| {
            object.insert("schemaVersion".into(), 4.into());
        }))
        .is_err()
    );
    assert!(
        decoded(&mutate(&json, |object| {
            object.insert("platform".into(), "windows".into());
        }))
        .is_err()
    );
    // Serde must reject duplicate fields as well as unknown fields.
    let duplicate = json.replacen('{', "{\"unixRoot\":\"/tmp/c2-other\",", 1);
    assert_rejected(&duplicate, &[EndpointCredentialErrorKind::MalformedJson]);
}

#[cfg(unix)]
#[test]
fn context_codec_v2_cannot_carry_custom_context_fields_even_as_null() {
    let context = LocalEndpointContext::default_for_platform().unwrap();
    let json = described_credential(&context).to_json().unwrap();
    for field in ["unixRoot", "namespaceId"] {
        for value in [serde_json::Value::Null, "/tmp/c2-other".into()] {
            assert_eq!(
                decoded(&mutate(&json, |object| {
                    object.insert(field.into(), value);
                }))
                .unwrap_err()
                .field(),
                Some(field),
            );
        }
    }
    assert!(
        decoded(&mutate(&json, |object| {
            object.insert("schemaVersion".into(), 3.into());
        }))
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn context_scope_derives_every_target_from_the_captured_context() {
    let context =
        LocalEndpointContext::with_unix_root(std::path::Path::new("/tmp/c2-scope")).unwrap();
    let endpoint = context.endpoint("ipc://scope-anchor").unwrap();
    let addresses = vec!["ipc://scope-a".to_owned(), "ipc://scope-b".to_owned()];
    let scope = EndpointSweepScope::from_addresses(&endpoint, &addresses).unwrap();
    assert_eq!(scope.endpoint, endpoint);
    for (target, address) in scope.targets.iter().zip(addresses.iter()) {
        assert_eq!(target, &context.endpoint(address).unwrap());
        assert_eq!(target.context(), &context);
    }
    assert!(
        EndpointSweepScope::from_addresses(&endpoint, &[])
            .unwrap()
            .targets
            .is_empty()
    );
    assert!(EndpointSweepScope::from_addresses(&endpoint, &["ipc://../outside".into()]).is_err());
    assert!(
        EndpointSweepScope::from_addresses(
            &endpoint,
            &vec![addresses[0].clone(); EndpointSweepScope::MAX_ADDRESSES + 1]
        )
        .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn context_codec_system_alias_keeps_its_lexical_identity() {
    let alias = LocalEndpointContext::with_unix_root(std::path::Path::new("/private/tmp")).unwrap();
    let default = LocalEndpointContext::default_for_platform().unwrap();
    assert_ne!(alias, default);
    let credential = described_credential(&alias);
    let value: serde_json::Value = serde_json::from_str(&credential.to_json().unwrap()).unwrap();
    assert_eq!(value["schemaVersion"], 3);
    assert_eq!(value["unixRoot"], "/private/tmp");
    assert_eq!(decoded(&value.to_string()).unwrap(), credential);
    assert!(
        default
            .endpoint("ipc://codec-context")
            .unwrap()
            .os_name()
            .to_str()
            .unwrap()
            .starts_with("/tmp/")
    );
}

#[test]
fn context_codec_rejects_unix_fields_in_windows_documents() {
    // These cases run on both platforms. Unix rejects the platform; Windows
    // must reject the otherwise known fields even when their value is null.
    for schema in [1, 3] {
        for fields in [
            r#""unixRoot":"/tmp/c2-codec""#,
            r#""namespaceId":"forged""#,
            r#""unixRoot":null"#,
            r#""namespaceId":null"#,
        ] {
            let json = format!(
                r#"{{"schemaVersion":{schema},"address":"ipc://codec-context","protocol":"named-pipe","platform":"windows",{fields}}}"#
            );
            assert!(decoded(&json).is_err());
        }
    }
    for field in [
        "incarnation",
        "device",
        "inode",
        "changedSecs",
        "changedNanos",
    ] {
        let json = format!(
            r#"{{"schemaVersion":1,"address":"ipc://codec-context","protocol":"named-pipe","platform":"windows","{field}":null}}"#
        );
        assert!(decoded(&json).is_err());
    }
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
    let document = r#"{"schemaVersion":1,"address":"ipc://codec-badplatform","protocol":"legacy-v1","platform":"unix"}"#;
    assert_rejected(
        &document,
        &[EndpointCredentialErrorKind::UnsupportedPlatform],
    );
}

#[cfg(unix)]
#[test]
fn unix_rejects_a_windows_platform_document() {
    let document = r#"{"schemaVersion":1,"address":"ipc://codec-badplatform","protocol":"named-pipe","platform":"windows"}"#;
    assert_rejected(
        &document,
        &[EndpointCredentialErrorKind::UnsupportedPlatform],
    );
}

/// Windows derives a Named Pipe, then rejects incompatible credential format
/// metadata before interpreting Unix incarnation or identity fields.
#[cfg(windows)]
#[test]
fn windows_rejects_managed_v2_documents() {
    let with_incarnation = r#"{"schemaVersion":2,"address":"ipc://codec-managedunsupported","protocol":"managed-v2","platform":"windows","incarnation":"00112233445566778899aabbccddeeff"}"#;
    let error = decoded(with_incarnation).expect_err("Windows rejects Unix credential metadata");
    assert_eq!(error.kind(), EndpointCredentialErrorKind::InvalidValue);
    assert_eq!(error.field(), Some("protocol"));
    // A Unix identity field is not a Windows pipe property either.
    let with_identity = r#"{"schemaVersion":1,"address":"ipc://codec-managedunsupported","protocol":"named-pipe","platform":"windows","device":1,"inode":2}"#;
    assert_rejected(with_identity, &[EndpointCredentialErrorKind::InvalidValue]);
}
