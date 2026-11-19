use super::*;
use std::fs;

fn env(entries: &[(&str, &str)]) -> EnvMap {
    entries
        .iter()
        .map(|&(key, value)| (key.into(), value.into()))
        .collect()
}

fn resolve_local_endpoint(
    options: LocalEndpointOptions,
    sources: ConfigSources,
) -> Result<LocalEndpointContext, ConfigError> {
    ConfigResolver::resolve_local_endpoint(options, &EnvCatalog::load(sources)?)
}

#[cfg(unix)]
#[test]
fn local_endpoint_resolution_follows_code_env_file_default_precedence() {
    use crate::{LocalEndpointContext, LocalEndpointOptions};

    let tempdir = tempfile::tempdir().expect("tempdir");
    let env_file = tempdir.path().join(".env");
    fs::write(&env_file, "C2_IPC_ROOT=/tmp/c2-file-root\n").expect("write env file");

    // Default: no code option, no env, no env file.
    let default_ctx =
        resolve_local_endpoint(LocalEndpointOptions::default(), ConfigSources::empty())
            .expect("default context should resolve");
    assert_eq!(
        default_ctx.unix_root(),
        LocalEndpointContext::default_for_platform()
            .unwrap()
            .unix_root(),
        "the resolver must use the platform default directory"
    );
    assert_eq!(
        default_ctx,
        resolve_local_endpoint(LocalEndpointOptions::default(), ConfigSources::empty())
            .expect("default context is stable")
    );

    // .env only.
    let from_file = resolve_local_endpoint(
        LocalEndpointOptions::default(),
        ConfigSources {
            env_file: EnvFilePolicy::Path(env_file.clone()),
            process_env: env(&[]),
        },
    )
    .expect("env-file context should resolve");
    assert_eq!(
        from_file.unix_root(),
        Some(std::path::Path::new("/tmp/c2-file-root"))
    );

    // Process env beats the env file.
    let from_env = resolve_local_endpoint(
        LocalEndpointOptions::default(),
        ConfigSources {
            env_file: EnvFilePolicy::Path(env_file.clone()),
            process_env: env(&[("C2_IPC_ROOT", "/tmp/c2-env-root")]),
        },
    )
    .expect("env context should resolve");
    assert_eq!(
        from_env.unix_root(),
        Some(std::path::Path::new("/tmp/c2-env-root"))
    );

    // Explicit code beats process env.
    let from_code = resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: Some(std::path::PathBuf::from("/tmp/c2-code-root")),
        },
        ConfigSources {
            env_file: EnvFilePolicy::Path(env_file),
            process_env: env(&[("C2_IPC_ROOT", "/tmp/c2-env-root")]),
        },
    )
    .expect("code context should resolve");
    assert_eq!(
        from_code.unix_root(),
        Some(std::path::Path::new("/tmp/c2-code-root"))
    );

    // Distinct roots keep the same logical address isolated and give it
    // distinct namespace identities.
    let address = "ipc://resolver-name";
    let env_endpoint = from_env.endpoint(address).expect("env endpoint");
    let code_endpoint = from_code.endpoint(address).expect("code endpoint");
    assert_ne!(env_endpoint, code_endpoint);
    assert_ne!(
        env_endpoint.context().namespace_id(),
        code_endpoint.context().namespace_id()
    );
    assert_eq!(
        from_env.namespace_id(),
        LocalEndpointContext::with_unix_root(std::path::Path::new("/tmp/c2-env-root"))
            .expect("same root is one identity")
            .namespace_id()
    );
}

#[cfg(unix)]
#[test]
fn local_endpoint_resolver_rejects_invalid_root_values() {
    use crate::LocalEndpointOptions;

    // "" is the only value rejected as explicitly empty; "   " fails the
    // native absolute-path check instead (never trimmed into validity).
    let empty_sources = ConfigSources {
        env_file: EnvFilePolicy::Disabled,
        process_env: env(&[("C2_IPC_ROOT", "")]),
    };
    let empty_err = resolve_local_endpoint(LocalEndpointOptions::default(), empty_sources)
        .expect_err("empty root should fail");
    assert!(
        empty_err.to_string().contains("cannot be empty"),
        "{empty_err}"
    );

    for value in ["   ", "relative/root", "/tmp/../escape", "/tmp/a\0b"] {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_ROOT", value)]),
        };
        let err = resolve_local_endpoint(LocalEndpointOptions::default(), sources)
            .expect_err(&format!("invalid root {value:?} should fail"));
        assert!(
            err.to_string().contains("C2_IPC_ROOT"),
            "error should name C2_IPC_ROOT for {value:?}: {err}"
        );
    }
}

#[cfg(unix)]
#[test]
fn local_endpoint_env_root_is_preserved_verbatim() {
    use crate::LocalEndpointOptions;

    // A trailing space is a legal Unix directory name: the env value must
    // resolve to exactly the same context/endpoint/namespace id as the
    // identical code-level path, and stay distinct from the name without
    // the trailing space.
    let spaced = "/tmp/c2-r ";
    let from_env = resolve_local_endpoint(
        LocalEndpointOptions::default(),
        ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_ROOT", spaced)]),
        },
    )
    .expect("spaced env root should resolve");
    let from_code = resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: Some(std::path::PathBuf::from(spaced)),
        },
        ConfigSources::empty(),
    )
    .expect("spaced code root should resolve");
    assert_eq!(from_env, from_code);
    assert_eq!(from_env.unix_root(), Some(std::path::Path::new(spaced)));
    assert_eq!(from_env.namespace_id(), from_code.namespace_id());

    let endpoint = from_env.endpoint("ipc://verbatim").expect("endpoint");
    assert_eq!(
        endpoint,
        from_code.endpoint("ipc://verbatim").expect("endpoint")
    );
    assert!(
        endpoint
            .os_name()
            .to_str()
            .unwrap()
            .starts_with("/tmp/c2-r /"),
        "{endpoint:?}"
    );

    let plain = resolve_local_endpoint(
        LocalEndpointOptions::default(),
        ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_ROOT", "/tmp/c2-r")]),
        },
    )
    .expect("plain env root should resolve");
    assert_ne!(from_env, plain);
    assert_ne!(from_env.namespace_id(), plain.namespace_id());
}

#[cfg(unix)]
#[test]
fn local_endpoint_env_root_whitespace_is_rejected_without_trimming() {
    use crate::LocalEndpointOptions;

    // Whitespace-only and leading-whitespace values keep their raw bytes
    // and fail the native absolute-path check; they must never be
    // trimmed into valid paths.
    for value in [" ", "  /tmp/c2-r", "\t/tmp/c2-r"] {
        let sources = ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: env(&[("C2_IPC_ROOT", value)]),
        };
        let err = resolve_local_endpoint(LocalEndpointOptions::default(), sources)
            .expect_err(&format!("whitespace root {value:?} must not become valid"));
        assert!(
            err.to_string().contains("C2_IPC_ROOT"),
            "error should name C2_IPC_ROOT for {value:?}: {err}"
        );
    }
}

#[cfg(unix)]
#[test]
fn local_endpoint_env_file_quoted_spaces_are_preserved() {
    use crate::LocalEndpointOptions;

    // Quoted .env values legally carry spaces; the parser must keep them
    // verbatim so the resolved context matches the same code-level path.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let env_file = tempdir.path().join(".env");
    fs::write(&env_file, "C2_IPC_ROOT=\"/tmp/c2-r quoted\"\n").expect("write env file");

    let from_file = resolve_local_endpoint(
        LocalEndpointOptions::default(),
        ConfigSources {
            env_file: EnvFilePolicy::Path(env_file),
            process_env: env(&[]),
        },
    )
    .expect("quoted env-file root should resolve");
    assert_eq!(
        from_file.unix_root(),
        Some(std::path::Path::new("/tmp/c2-r quoted"))
    );

    let from_code = resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: Some(std::path::PathBuf::from("/tmp/c2-r quoted")),
        },
        ConfigSources::empty(),
    )
    .expect("quoted code root should resolve");
    assert_eq!(from_file, from_code);
    assert_eq!(from_file.namespace_id(), from_code.namespace_id());
}
