#[cfg(unix)]
use c2_config::{ConfigSources, EnvFilePolicy, EnvMap, LocalEndpointOptions};
#[cfg(unix)]
use c2_core::{Runtime, RuntimeOptions};

#[cfg(unix)]
#[test]
fn unused_selection_preserves_code_priority_and_native_sources() {
    let previous = Runtime::new(RuntimeOptions::default()).unwrap();
    previous
        .set_local_endpoint_with_sources(
            LocalEndpointOptions {
                unix_root: Some("/tmp/c2-code".into()),
            },
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: EnvMap::from([("C2_IPC_ROOT".into(), "relative".into())]),
            },
        )
        .unwrap();
    let next = Runtime::new(RuntimeOptions::default()).unwrap();
    next.inherit_local_endpoint_selection(&previous).unwrap();
    assert!(!previous.local_endpoint_frozen());
    assert!(!next.local_endpoint_frozen());
    assert_eq!(
        next.local_endpoint_context().unwrap().unix_root().unwrap(),
        std::path::Path::new("/tmp/c2-code")
    );
    // A rejected configuration still leaves the accepted code choice intact.
    assert!(
        next.set_local_endpoint_with_sources(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: EnvMap::from([("C2_IPC_ROOT".into(), "relative".into())]),
            }
        )
        .is_err()
    );
    assert_eq!(
        next.local_endpoint_context().unwrap(),
        previous.local_endpoint_context().unwrap()
    );
    next.set_local_endpoint_with_sources(
        LocalEndpointOptions {
            unix_root: Some("/tmp/c2-changed".into()),
        },
        ConfigSources::empty(),
    )
    .unwrap();
    assert_ne!(
        next.local_endpoint_context().unwrap(),
        previous.local_endpoint_context().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn unused_selection_preserves_explicit_native_sources() {
    let previous = Runtime::new(RuntimeOptions::default()).unwrap();
    previous
        .set_local_endpoint_with_sources(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: EnvMap::from([("C2_IPC_ROOT".into(), "/tmp/c2-native-source".into())]),
            },
        )
        .unwrap();
    let next = Runtime::new(RuntimeOptions::default()).unwrap();
    next.inherit_local_endpoint_selection(&previous).unwrap();
    assert!(!next.local_endpoint_frozen());
    assert_eq!(
        next.local_endpoint_context().unwrap().unix_root().unwrap(),
        std::path::Path::new("/tmp/c2-native-source")
    );
}
