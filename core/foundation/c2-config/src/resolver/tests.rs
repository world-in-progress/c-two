use super::*;
use std::fs;

fn sources(entries: &[(&str, &str)]) -> ConfigSources {
    ConfigSources {
        process_env: entries
            .iter()
            .map(|&(key, value)| (key.into(), value.into()))
            .collect(),
        ..ConfigSources::empty()
    }
}

fn catalog(entries: &[(&str, &str)]) -> EnvCatalog {
    EnvCatalog::load(sources(entries)).unwrap()
}

#[test]
fn one_catalog_captures_file_and_process_values_for_all_roles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.env");
    fs::write(
        &path,
        "C2_IPC_ROOT=/tmp/c2-captured
C2_RELAY_ANCHOR_ADDRESS=http://anchor:8080
C2_MEMORY_BUDGET_BYTES=10485760
C2_FILE_SPILL_BUDGET_BYTES=98765
C2_CALL_MAX_OUTSTANDING=12
C2_MAX_PAYLOAD_SIZE=123456
C2_MAX_EXECUTION_WORKERS=100
C2_RELAY_BIND=127.0.0.1:9000
C2_RELAY_ID=relay-a
C2_RELAY_ADVERTISE_URL=http://relay-a:9000
C2_RELAY_SEEDS=\"http://relay-b:9000, http://relay-c:9000\"
C2_RELAY_USE_PROXY=yes
",
    )
    .unwrap();
    let before = ENV_FILE_LOADS.with(std::cell::Cell::get);
    let catalog = EnvCatalog::load(ConfigSources {
        env_file: EnvFilePolicy::FromC2EnvFile,
        process_env: EnvMap::from([
            (ENV_FILE.into(), path.into_os_string()),
            (FILE_SPILL_BUDGET.into(), "54321".into()),
        ]),
    })
    .unwrap();
    fs::remove_file(dir.path().join("runtime.env")).unwrap();

    let server = ConfigResolver::resolve_server(
        &ServerConfigOverrides {
            domain_limits: DomainLimitsOverrides {
                max_outstanding_calls: Some(13),
                ..Default::default()
            },
            ..Default::default()
        },
        &catalog,
    )
    .unwrap();
    assert_eq!(server.ipc.max_payload_size, 123456);
    assert_eq!(server.ipc.max_execution_workers, 100);
    assert_eq!(server.domain_limits.max_outstanding_calls, 13);

    let domain = ConfigResolver::resolve_domain_limits(&Default::default(), &catalog).unwrap();
    assert_eq!(
        domain,
        DomainLimits {
            memory_budget_bytes: 10485760,
            file_spill_budget_bytes: 54321,
            max_outstanding_calls: 12,
        }
    );
    let relay = ConfigResolver::resolve_relay_server(Default::default(), &catalog).unwrap();
    assert_eq!(relay.domain_limits, domain);
    assert_eq!(relay.bind, "127.0.0.1:9000");
    assert_eq!(relay.relay_id, "relay-a");
    assert_eq!(relay.effective_advertise_url(), "http://relay-a:9000");
    assert_eq!(relay.seeds, ["http://relay-b:9000", "http://relay-c:9000"]);
    assert!(relay.use_proxy);
    assert!(
        !ConfigResolver::resolve_relay_client(Some(false), &catalog)
            .unwrap()
            .use_proxy
    );
    assert_eq!(
        ConfigResolver::resolve_relay_anchor_address(&catalog)
            .unwrap()
            .as_deref(),
        Some("http://anchor:8080")
    );
    let endpoint = ConfigResolver::resolve_local_endpoint(Default::default(), &catalog);
    #[cfg(unix)]
    assert_eq!(
        endpoint.unwrap().unix_root(),
        Some(std::path::Path::new("/tmp/c2-captured"))
    );
    #[cfg(windows)]
    assert!(endpoint.unwrap_err().to_string().contains(IPC_ROOT));
    assert_eq!(ENV_FILE_LOADS.with(std::cell::Cell::get) - before, 1);
}

#[test]
fn retired_environment_does_not_enter_catalog_and_empty_env_file_disables_loading() {
    let before = ENV_FILE_LOADS.with(std::cell::Cell::get);
    let catalog = EnvCatalog::load(ConfigSources {
        env_file: EnvFilePolicy::FromC2EnvFile,
        process_env: sources(&[
            (ENV_FILE, ""),
            ("C2_SHM_THRESHOLD", "1"),
            ("C2_IPC_MAX_PAYLOAD_SIZE", "0"),
            ("C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES", "0"),
            ("C2_RELAY_CALL_TIMEOUT", "invalid"),
        ])
        .process_env,
    })
    .unwrap();
    assert!(catalog.env.is_empty());
    assert_eq!(ENV_FILE_LOADS.with(std::cell::Cell::get), before);
    assert_eq!(
        ConfigResolver::resolve_domain_limits(&Default::default(), &catalog).unwrap(),
        DomainLimits::default()
    );
    let server = ConfigResolver::resolve_server(&Default::default(), &catalog).unwrap();
    assert_eq!(server.ipc.max_payload_size, 16 * (1 << 30));
    assert!((4..=64).contains(&server.ipc.max_execution_workers));
    assert_eq!(crate::SHM_THRESHOLD, 16384);
}

#[test]
fn malformed_numeric_values_name_the_effective_environment_source() {
    for (key, is_server) in [
        (MEMORY_BUDGET, false),
        (FILE_SPILL_BUDGET, false),
        (CALL_MAX_OUTSTANDING, false),
        (MAX_PAYLOAD, true),
        (EXECUTION_WORKERS, true),
    ] {
        for value in ["", "   ", "-1", "invalid", "18446744073709551616"] {
            let catalog = catalog(&[(key, value)]);
            let error = if is_server {
                ConfigResolver::resolve_server(&Default::default(), &catalog).unwrap_err()
            } else {
                ConfigResolver::resolve_domain_limits(&Default::default(), &catalog).unwrap_err()
            };
            assert!(error.to_string().contains(key), "{error}");
        }
    }
    for key in [MAX_PAYLOAD, EXECUTION_WORKERS] {
        let error = ConfigResolver::resolve_server(&Default::default(), &catalog(&[(key, "0")]))
            .unwrap_err();
        assert!(error.to_string().contains(key), "{error}");
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capacity.env");
    fs::write(&path, "C2_MEMORY_BUDGET_BYTES=45\n").unwrap();
    let catalog = EnvCatalog::load(ConfigSources {
        env_file: EnvFilePolicy::Path(path),
        process_env: sources(&[(MEMORY_BUDGET, "")]).process_env,
    })
    .unwrap();
    assert!(ConfigResolver::resolve_domain_limits(&Default::default(), &catalog).is_err());
    assert!(
        ConfigResolver::resolve_domain_limits(
            &DomainLimitsOverrides {
                memory_budget_bytes: Some(45),
                ..Default::default()
            },
            &catalog
        )
        .is_ok()
    );
}

#[test]
fn domain_defaults_and_explicit_zero_follow_memory() {
    for (memory, slots) in [
        (0, 0),
        (1, 64),
        (65 * 65_536, 65),
        (16 * (1 << 30), 262_144),
    ] {
        let limits = ConfigResolver::resolve_domain_limits(
            &DomainLimitsOverrides {
                memory_budget_bytes: Some(memory),
                ..Default::default()
            },
            &catalog(&[]),
        )
        .unwrap();
        assert_eq!(limits.max_outstanding_calls, slots);
        assert_eq!(limits.file_spill_budget_bytes, 16 * (1 << 30));
    }
    for entries in [
        &[(MEMORY_BUDGET, "0")][..],
        &[(MEMORY_BUDGET, "0"), (CALL_MAX_OUTSTANDING, "0")][..],
        &[(MEMORY_BUDGET, "100"), (CALL_MAX_OUTSTANDING, "0")][..],
    ] {
        assert_eq!(
            ConfigResolver::resolve_domain_limits(&Default::default(), &catalog(entries))
                .unwrap()
                .max_outstanding_calls,
            0
        );
    }
    let error = ConfigResolver::resolve_domain_limits(
        &Default::default(),
        &catalog(&[(MEMORY_BUDGET, "0"), (CALL_MAX_OUTSTANDING, "1")]),
    )
    .unwrap_err();
    assert!(error.to_string().contains(CALL_MAX_OUTSTANDING));
}

#[test]
fn patches_commit_only_valid_complete_candidates() {
    let catalog = catalog(&[(CALL_MAX_OUTSTANDING, "5")]);
    let mut overrides = ServerConfigOverrides::default();
    let accepted = overrides
        .apply_patch(
            ServerConfigPatch {
                server_id: ConfigField::Set("host-a".into()),
                startup_timeout: ConfigField::Set(Duration::ZERO),
                domain_limits: DomainLimitsPatch {
                    max_outstanding_calls: ConfigField::Set(7),
                    ..Default::default()
                },
                ..Default::default()
            },
            &catalog,
        )
        .unwrap();
    assert_eq!(accepted.domain_limits.max_outstanding_calls, 7);
    assert_eq!(accepted.startup_timeout, Duration::ZERO);

    let saved = overrides.clone();
    assert!(
        overrides
            .apply_patch(
                ServerConfigPatch {
                    server_id: ConfigField::Inherit,
                    domain_limits: DomainLimitsPatch {
                        memory_budget_bytes: ConfigField::Set(0),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &catalog
            )
            .is_err()
    );
    assert_eq!(overrides, saved);

    let updated = overrides
        .apply_patch(
            ServerConfigPatch {
                domain_limits: DomainLimitsPatch {
                    memory_budget_bytes: ConfigField::Set(123),
                    max_outstanding_calls: ConfigField::Inherit,
                    ..Default::default()
                },
                ..Default::default()
            },
            &catalog,
        )
        .unwrap();
    assert_eq!(updated.server_id.as_deref(), Some("host-a"));
    assert_eq!(updated.domain_limits.max_outstanding_calls, 5);

    let saved = overrides.domain_limits;
    assert!(
        overrides
            .domain_limits
            .apply_patch(
                DomainLimitsPatch {
                    memory_budget_bytes: ConfigField::Set(0),
                    ..Default::default()
                },
                &catalog
            )
            .is_err()
    );
    assert_eq!(overrides.domain_limits, saved);
}

#[test]
fn outcome_store_limits_resolve_and_validate_against_memory_budget() {
    let empty = catalog(&[]);
    let default_server = ConfigResolver::resolve_server(&Default::default(), &empty).unwrap();
    assert_eq!(
        default_server.ipc.outcome_max_entries,
        crate::DEFAULT_OUTCOME_MAX_ENTRIES
    );
    assert_eq!(default_server.ipc.outcome_max_retained_bytes, None);
    assert_eq!(
        default_server
            .ipc
            .effective_outcome_max_retained_bytes(default_server.domain_limits.memory_budget_bytes),
        crate::DEFAULT_OUTCOME_MAX_RETAINED_BYTES_CAP
    );

    let small_mem = catalog(&[("C2_MEMORY_BUDGET_BYTES", "16777216")]);
    let resolved = ConfigResolver::resolve_server(&Default::default(), &small_mem).unwrap();
    assert_eq!(
        resolved
            .ipc
            .effective_outcome_max_retained_bytes(resolved.domain_limits.memory_budget_bytes),
        4 * 1024 * 1024
    );

    let metadata_only = catalog(&[
        ("C2_OUTCOME_MAX_ENTRIES", "128"),
        ("C2_OUTCOME_MAX_RETAINED_BYTES", "0"),
    ]);
    let resolved = ConfigResolver::resolve_server(&Default::default(), &metadata_only).unwrap();
    assert_eq!(resolved.ipc.outcome_max_entries, 128);
    assert_eq!(resolved.ipc.outcome_max_retained_bytes, Some(0));
    assert_eq!(
        resolved
            .ipc
            .effective_outcome_max_retained_bytes(resolved.domain_limits.memory_budget_bytes),
        0
    );

    let zero_entries = catalog(&[("C2_OUTCOME_MAX_ENTRIES", "0")]);
    assert!(
        ConfigResolver::resolve_server(&Default::default(), &zero_entries)
            .unwrap_err()
            .to_string()
            .contains("C2_OUTCOME_MAX_ENTRIES")
    );

    let exceeds_mem = catalog(&[
        ("C2_MEMORY_BUDGET_BYTES", "1048576"),
        ("C2_OUTCOME_MAX_RETAINED_BYTES", "2097152"),
    ]);
    assert!(
        ConfigResolver::resolve_server(&Default::default(), &exceeds_mem)
            .unwrap_err()
            .to_string()
            .contains("C2_OUTCOME_MAX_RETAINED_BYTES")
    );
}

fn relay_client_settings(vars: &[(&str, &str)]) -> Result<ResolvedRelayClientConfig, ConfigError> {
    ConfigResolver::resolve_relay_client(None, &catalog(vars))
}

#[test]
fn relay_tls_trust_defaults_to_builtin_roots_and_accepts_a_ca_path() {
    assert_eq!(
        relay_client_settings(&[]).unwrap(),
        ResolvedRelayClientConfig::default()
    );
    assert_eq!(
        relay_client_settings(&[(RELAY_CA_CERT, "my CA.pem"), (RELAY_USE_PROXY, "yes")]).unwrap(),
        ResolvedRelayClientConfig {
            use_proxy: true,
            tls_trust: RelayTlsTrust::CustomCaCert("my CA.pem".into()),
        }
    );
}

#[test]
fn relay_certificate_fingerprint_has_one_identity() {
    let digest = "ab".repeat(32);
    let prefixed = format!("sha256:{}", digest.to_ascii_uppercase());
    let bare = relay_client_settings(&[(RELAY_CERT_SHA256, &digest)]).unwrap();
    assert_eq!(bare.tls_trust, RelayTlsTrust::CertSha256([0xab; 32]));
    assert_eq!(
        bare,
        relay_client_settings(&[(RELAY_CERT_SHA256, &prefixed)]).unwrap()
    );
}

#[test]
fn relay_tls_rejects_ambiguous_or_malformed_configuration() {
    let pin = "ab".repeat(32);
    let invalid_hex = "ag".repeat(32);
    for (vars, expected) in [
        (
            vec![(RELAY_CA_CERT, "ca.pem"), (RELAY_CERT_SHA256, pin.as_str())],
            "mutually exclusive",
        ),
        (vec![(RELAY_CA_CERT, "")], RELAY_CA_CERT),
        (vec![(RELAY_CA_CERT, "  ")], RELAY_CA_CERT),
        (vec![(RELAY_CERT_SHA256, "")], RELAY_CERT_SHA256),
        (vec![(RELAY_CERT_SHA256, "sha256:abcd")], RELAY_CERT_SHA256),
        (
            vec![(RELAY_CERT_SHA256, invalid_hex.as_str())],
            RELAY_CERT_SHA256,
        ),
    ] {
        assert!(
            relay_client_settings(&vars)
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }
}

#[test]
fn relay_tls_requires_utf8() {
    #[cfg(unix)]
    let invalid = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    };
    #[cfg(windows)]
    let invalid = {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    };
    for key in [RELAY_CA_CERT, RELAY_CERT_SHA256] {
        let catalog = EnvCatalog::load(ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: [(key.to_owned(), invalid.clone())].into(),
        })
        .unwrap();
        assert!(
            ConfigResolver::resolve_relay_client(None, &catalog)
                .unwrap_err()
                .to_string()
                .contains(&format!("{key} must be valid UTF-8"))
        );
    }
}

#[test]
fn relay_server_resolves_edge_settings_and_rejects_conflicting_modes() {
    use crate::RelayEdgeTlsMode;

    let resolve =
        |entries| ConfigResolver::resolve_relay_server(Default::default(), &catalog(entries));
    let automatic = resolve(&[
        (RELAY_EDGE_BIND, "[::]:8443"),
        (RELAY_TLS_AUTO, "yes"),
        (RELAY_TLS_SAN, "relay.example, 192.0.2.1"),
        (RELAY_STATE_DIR, "edge-state"),
    ])
    .unwrap();
    assert_eq!(
        automatic.edge_tls_mode().unwrap(),
        Some(RelayEdgeTlsMode::AutoSelfSigned)
    );
    assert_eq!(automatic.tls_san, ["relay.example", "192.0.2.1"]);
    assert_eq!(automatic.state_dir, Some(PathBuf::from("edge-state")));
    assert_eq!(automatic.effective_advertise_url(), "https://[::1]:8443");

    let explicit = resolve(&[
        (RELAY_EDGE_BIND, "localhost:8443"),
        (RELAY_TLS_CERT, "cert.pem"),
        (RELAY_TLS_KEY, "key.pem"),
    ])
    .unwrap();
    assert_eq!(
        explicit.edge_tls_mode().unwrap(),
        Some(RelayEdgeTlsMode::ExplicitCert)
    );
    assert_eq!(explicit.tls_cert, Some(PathBuf::from("cert.pem")));
    assert_eq!(explicit.tls_key, Some(PathBuf::from("key.pem")));

    for options in [
        &[(RELAY_BIND, "0.0.0.0:8080")][..],
        &[(RELAY_TLS_AUTO, "true")][..],
        &[(RELAY_INSECURE_PLAINTEXT, "yes")][..],
        &[(RELAY_TLS_SAN, "relay.example")][..],
        &[(RELAY_STATE_DIR, "edge-state")][..],
        &[
            (RELAY_EDGE_BIND, "localhost:8443"),
            (RELAY_TLS_CERT, "cert.pem"),
        ][..],
        &[
            (RELAY_EDGE_BIND, "localhost:8443"),
            (RELAY_TLS_AUTO, "true"),
            (RELAY_INSECURE_PLAINTEXT, "true"),
        ][..],
        &[(RELAY_ADVERTISE_URL, "http://0.0.0.0:8080")][..],
    ] {
        assert!(resolve(options).is_err(), "{options:?}");
    }
}
