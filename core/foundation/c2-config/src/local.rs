//! Logical local addresses and their operating-system endpoint names.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

/// Native OS namespace used by an endpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LocalEndpointNamespace {
    UnixFilesystem,
    WindowsNamedPipe,
}

/// Typed local endpoint resolution options.
///
/// `unix_root` is a Unix platform capability. The Windows named-pipe backend
/// rejects it as not applicable instead of silently ignoring it; the OS alone
/// selects the backend and there is no user-facing protocol selector.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LocalEndpointOptions {
    /// Absolute final endpoint directory. A custom directory is provisioned
    /// by the application with private permissions; the platform default can
    /// be initialized by C-Two. Resolution never touches the filesystem.
    pub unix_root: Option<PathBuf>,
}

#[cfg(windows)]
const UNIX_ROOT_NOT_APPLICABLE: &str =
    "unix root override is not applicable to the Windows named-pipe backend";
/// Domain tag binding every namespace id to this exact identity scheme.
const NAMESPACE_ID_DOMAIN: &str = "c2.local-endpoint-namespace.v1";

/// Immutable platform endpoint context: the resolved root/scope identity a
/// runtime freezes before its first local bind/connect.
///
/// Endpoint derivation, cache identity, and relay namespace comparison all
/// key on this value, so two roots serving the same logical addresses stay
/// isolated. Construction is pure validation: it never reads the target
/// directory, creates files, or consults the process environment afterwards.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocalEndpointContext {
    platform: LocalEndpointPlatform,
    namespace_id: String,
}

/// Platform identity behind every derived local endpoint.
///
/// Both variants are defined on every platform so Core code can match
/// exhaustively, but a value can only be produced by the validated
/// constructors of the running platform: a fake root or uid can never back a
/// Windows public identity, and the Windows backend can never be selected on
/// Unix. The foreign variant is therefore intentionally never constructed.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum LocalEndpointPlatform {
    UnixManaged {
        /// Normalized absolute root, validated UTF-8.
        root: String,
        uid: u32,
    },
    WindowsNamedPipe {
        logon_scope_id: String,
    },
}

impl LocalEndpointPlatform {
    const fn kind(&self) -> LocalEndpointNamespace {
        match self {
            Self::UnixManaged { .. } => LocalEndpointNamespace::UnixFilesystem,
            Self::WindowsNamedPipe { .. } => LocalEndpointNamespace::WindowsNamedPipe,
        }
    }

    fn derive_os_name(&self, server_id: &str) -> io::Result<OsString> {
        let identity = endpoint_identity(server_id);
        match self {
            #[cfg(unix)]
            Self::UnixManaged { root, .. } => {
                Ok(PathBuf::from(root).join(identity).into_os_string())
            }
            #[cfg(windows)]
            Self::UnixManaged { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unix filesystem endpoints cannot be derived on the Windows named-pipe backend",
            )),
            #[cfg(windows)]
            Self::WindowsNamedPipe { logon_scope_id, .. } => {
                Ok(format!(r"\\.\pipe\c_two-{logon_scope_id}-{identity}").into())
            }
            #[cfg(unix)]
            Self::WindowsNamedPipe { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows named-pipe endpoints cannot be derived on the Unix filesystem backend",
            )),
        }
    }
}

impl LocalEndpointContext {
    /// Context for the platform default: `/tmp/c2-<uidhex>` on
    /// Unix, the current logon scope on Windows. Pure derivation; it never
    /// reads `C2_IPC_ROOT` or any other environment variable.
    pub fn default_for_platform() -> io::Result<Self> {
        #[cfg(windows)]
        {
            let logon_scope_id = c2_local_security::current_scope_id()?;
            return Ok(Self::new(LocalEndpointPlatform::WindowsNamedPipe {
                logon_scope_id,
            }));
        }

        #[cfg(unix)]
        {
            let uid = unsafe { libc::geteuid() };
            return Self::with_unix_root(Path::new(&format!("/tmp/c2-{uid:x}")));
        }
    }

    /// Context under a user-provided absolute Unix root. The path is
    /// validated and normalized without touching the filesystem; a custom
    /// directory must already exist by the time an endpoint is actually
    /// bound. On Windows this is rejected as not applicable.
    pub fn with_unix_root(root: &Path) -> io::Result<Self> {
        #[cfg(windows)]
        {
            let _ = root;
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                UNIX_ROOT_NOT_APPLICABLE,
            ));
        }

        #[cfg(unix)]
        {
            let root = validate_unix_root(root)?;
            let uid = unsafe { libc::geteuid() };
            return Ok(Self::new(LocalEndpointPlatform::UnixManaged { root, uid }));
        }
    }

    fn new(platform: LocalEndpointPlatform) -> Self {
        let namespace_id = compute_namespace_id(&platform);
        Self {
            platform,
            namespace_id,
        }
    }

    /// The OS backend this context was constructed for.
    pub const fn platform_kind(&self) -> LocalEndpointNamespace {
        self.platform.kind()
    }

    /// Normalized Unix root when this is a `UnixManaged` context.
    pub fn unix_root(&self) -> Option<&Path> {
        match &self.platform {
            LocalEndpointPlatform::UnixManaged { root, .. } => Some(Path::new(root.as_str())),
            LocalEndpointPlatform::WindowsNamedPipe { .. } => None,
        }
    }

    /// Effective uid baked into a `UnixManaged` context.
    pub fn unix_uid(&self) -> Option<u32> {
        match &self.platform {
            LocalEndpointPlatform::UnixManaged { uid, .. } => Some(*uid),
            LocalEndpointPlatform::WindowsNamedPipe { .. } => None,
        }
    }

    /// Windows logon scope when this is a `WindowsNamedPipe` context.
    pub fn windows_logon_scope_id(&self) -> Option<&str> {
        match &self.platform {
            LocalEndpointPlatform::UnixManaged { .. } => None,
            LocalEndpointPlatform::WindowsNamedPipe { logon_scope_id, .. } => Some(logon_scope_id),
        }
    }

    /// Stable cross-process namespace identifier: a domain-separated SHA-256
    /// over the backend kind and platform scope (normalized
    /// Unix root plus effective uid, or Windows logon scope), with
    /// length-prefixed component boundaries. It contains no raw path bytes,
    /// never depends on the process pid, and is a routing/comparison key
    /// only — it grants no permission by itself.
    pub fn namespace_id(&self) -> &str {
        &self.namespace_id
    }

    /// Derive the endpoint for a logical IPC address within this context.
    ///
    /// Unix returns the full descriptive path. Local transport operations use
    /// the directory descriptor and short name, so `sun_path` does not limit
    /// the configured directory here.
    pub fn endpoint(&self, address: &str) -> io::Result<LocalEndpoint> {
        LocalEndpoint::in_context(self, address)
    }
}

#[cfg(unix)]
fn validate_unix_root(root: &Path) -> io::Result<String> {
    use std::os::unix::ffi::OsStrExt;

    let raw = root.as_os_str();
    if raw.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local endpoint root cannot be empty",
        ));
    }
    if raw.as_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local endpoint root cannot contain NUL bytes",
        ));
    }
    if raw.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local endpoint root must be valid UTF-8",
        ));
    }
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local endpoint root must be an absolute path",
        ));
    }
    for component in root.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::Normal(_) => {}
            std::path::Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local endpoint root cannot contain '..'",
                ));
            }
            // CurDir cannot appear inside an absolute component stream and
            // Prefix does not exist on Unix; reject defensively.
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local endpoint root contains an unsupported path component",
                ));
            }
        }
    }
    // Normalize away duplicate separators and trailing slashes so identity
    // comparisons treat "/tmp/x/" and "/tmp/x" as one root.
    let normalized: PathBuf = root.components().collect();
    let normalized_text = normalized.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "local endpoint root must be valid UTF-8",
        )
    })?;
    Ok(normalized_text.to_owned())
}

fn compute_namespace_id(platform: &LocalEndpointPlatform) -> String {
    use sha2::{Digest, Sha256};

    fn feed(hasher: &mut Sha256, field: &[u8]) {
        hasher.update(u64::to_le_bytes(field.len() as u64));
        hasher.update(field);
    }

    let mut hasher = Sha256::new();
    feed(&mut hasher, NAMESPACE_ID_DOMAIN.as_bytes());
    match platform {
        LocalEndpointPlatform::UnixManaged { root, uid } => {
            feed(&mut hasher, b"unix-filesystem");
            feed(&mut hasher, root.as_bytes());
            feed(&mut hasher, &uid.to_le_bytes());
        }
        LocalEndpointPlatform::WindowsNamedPipe { logon_scope_id } => {
            feed(&mut hasher, b"windows-named-pipe");
            feed(&mut hasher, logon_scope_id.as_bytes());
        }
    }
    let mut id = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut id, "{byte:02x}").expect("writing to String cannot fail");
    }
    id
}

/// A validated local IPC endpoint. The OS name is not a filesystem existence
/// probe: on Windows it names a pipe in the kernel namespace. Every endpoint
/// carries the immutable context it was derived from, so caches and
/// maintenance scopes can distinguish identical logical names in two roots.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocalEndpoint {
    address: String,
    os_name: OsString,
    context: LocalEndpointContext,
}

impl LocalEndpoint {
    /// Pure platform-default derivation from `/tmp/c2-<uidhex>` (Unix) or the current
    /// logon scope (Windows). It does not read the process environment;
    /// environment-resolved roots go through `ConfigResolver` and a frozen
    /// `LocalEndpointContext`.
    pub fn from_address(address: &str) -> io::Result<Self> {
        let context = LocalEndpointContext::default_for_platform()?;
        context.endpoint(address)
    }

    /// Validate only the logical address, without resolving or deriving an
    /// endpoint. Admin probes use this to distinguish malformed targets from
    /// failures of a valid target's configured platform namespace.
    pub fn validate_address(address: &str) -> io::Result<&str> {
        let server_id = address.strip_prefix("ipc://").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid IPC address: {address}"),
            )
        })?;
        crate::validate_ipc_region_id(server_id)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(server_id)
    }

    fn in_context(context: &LocalEndpointContext, address: &str) -> io::Result<Self> {
        let server_id = Self::validate_address(address)?;
        let os_name = context.platform.derive_os_name(server_id)?;
        Ok(Self {
            address: address.to_owned(),
            os_name,
            context: context.clone(),
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn os_name(&self) -> &OsStr {
        &self.os_name
    }

    /// The immutable context this endpoint was derived from.
    pub fn context(&self) -> &LocalEndpointContext {
        &self.context
    }

    /// Strict credential metadata for the platform's native backend.
    pub const fn protocol(&self) -> &'static str {
        match self.context.platform_kind() {
            LocalEndpointNamespace::UnixFilesystem => "managed-v2",
            LocalEndpointNamespace::WindowsNamedPipe => "named-pipe",
        }
    }

    pub const fn namespace(&self) -> LocalEndpointNamespace {
        self.context.platform_kind()
    }

    pub fn transport_kind(&self) -> &'static str {
        match self.context.platform_kind() {
            LocalEndpointNamespace::UnixFilesystem => "unix-socket",
            LocalEndpointNamespace::WindowsNamedPipe => "named-pipe",
        }
    }
}

fn endpoint_identity(server_id: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut identity = String::with_capacity(32);
    for byte in &Sha256::digest(server_id.as_bytes())[..16] {
        write!(&mut identity, "{byte:02x}").expect("writing to String cannot fail");
    }
    identity
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_map(entries: &[(&str, &str)]) -> crate::EnvMap {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn rejects_nonlocal_and_path_like_addresses() {
        for address in [
            "tcp://host",
            "ipc://",
            "ipc://..",
            "ipc://x/y",
            "ipc://x\\y",
            "ipc://x\n",
        ] {
            assert_eq!(
                LocalEndpoint::from_address(address).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn logical_identity_is_preserved_and_names_are_deterministic() {
        let a = LocalEndpoint::from_address("ipc://Server-A").unwrap();
        assert_eq!(a.address(), "ipc://Server-A");
        assert_eq!(a, LocalEndpoint::from_address("ipc://Server-A").unwrap());
        assert_ne!(
            a.os_name(),
            LocalEndpoint::from_address("ipc://server-a")
                .unwrap()
                .os_name()
        );
    }

    #[test]
    fn endpoint_names_use_the_first_128_sha256_bits() {
        for (server_id, digest) in [
            ("Server-A", "1118fcf083aa343ac0caf420a35a3025"),
            ("server-a", "a79b8498a1fb0114738f243cfd7c1eea"),
            ("资源-Server-A", "4ad7b4ee929ad2c148664fcaa37c16e8"),
        ] {
            let endpoint = LocalEndpoint::from_address(&format!("ipc://{server_id}")).unwrap();
            #[cfg(unix)]
            let expected = format!("/tmp/c2-{:x}/{digest}", unsafe { libc::geteuid() });
            #[cfg(windows)]
            let expected = format!(
                r"\\.\pipe\c_two-{}-{digest}",
                c2_local_security::current_scope_id().unwrap()
            );
            assert_eq!(endpoint.os_name(), OsStr::new(&expected));
        }
    }

    #[test]
    fn default_context_identity_is_stable_and_hex_only() {
        let a = LocalEndpointContext::default_for_platform().unwrap();
        let b = LocalEndpointContext::default_for_platform().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.namespace_id(), b.namespace_id());
        assert_eq!(a.namespace_id().len(), 64);
        assert!(a.namespace_id().chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.platform_kind(), b.platform_kind());
    }

    #[test]
    fn endpoint_round_trips_through_its_context() {
        let endpoint = LocalEndpoint::from_address("ipc://Server-A").unwrap();
        let replayed = endpoint.context().endpoint("ipc://Server-A").unwrap();
        assert_eq!(endpoint, replayed);
        assert_eq!(endpoint.context(), replayed.context());
        assert_eq!(endpoint.address(), replayed.address());
        assert_eq!(endpoint.os_name(), replayed.os_name());
    }

    #[cfg(unix)]
    #[test]
    fn default_context_derives_directly_in_the_private_directory() {
        use sha2::{Digest, Sha256};

        let context = LocalEndpointContext::default_for_platform().unwrap();
        assert_eq!(
            context.platform_kind(),
            LocalEndpointNamespace::UnixFilesystem
        );
        assert_eq!(
            context.unix_root(),
            Some(Path::new(&format!("/tmp/c2-{:x}", unsafe {
                libc::geteuid()
            })))
        );
        assert_eq!(context.unix_uid(), Some(unsafe { libc::geteuid() }));

        for server_id in ["Server-A", "server-a", "资源-Server-A"] {
            let digest: String = Sha256::digest(server_id.as_bytes())[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let expected = format!("/tmp/c2-{:x}/{digest}", unsafe { libc::geteuid() });
            let via_context = context.endpoint(&format!("ipc://{server_id}")).unwrap();
            let via_default = LocalEndpoint::from_address(&format!("ipc://{server_id}")).unwrap();
            assert_eq!(via_context.os_name(), OsStr::new(&expected));
            assert_eq!(via_context, via_default);
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_roots_isolate_the_same_logical_address() {
        let root_alpha = Path::new("/tmp/c2-it-根A alpha");
        let root_beta = Path::new("/tmp/c2-it-b");
        let alpha = LocalEndpointContext::with_unix_root(root_alpha).unwrap();
        let beta = LocalEndpointContext::with_unix_root(root_beta).unwrap();
        assert_ne!(alpha, beta);
        assert_ne!(alpha.namespace_id(), beta.namespace_id());

        let in_alpha = alpha.endpoint("ipc://shared-name").unwrap();
        let in_beta = beta.endpoint("ipc://shared-name").unwrap();
        assert_ne!(in_alpha, in_beta);
        assert_ne!(in_alpha.os_name(), in_beta.os_name());
        assert_ne!(
            in_alpha.context().namespace_id(),
            in_beta.context().namespace_id()
        );
        assert!(
            in_alpha
                .os_name()
                .to_str()
                .unwrap()
                .starts_with("/tmp/c2-it-根A alpha/")
        );
        assert_eq!(Path::new(in_alpha.os_name()).parent(), Some(root_alpha));
        assert_eq!(Path::new(in_alpha.os_name()).file_name().unwrap().len(), 32);
        assert_eq!(in_alpha.address(), "ipc://shared-name");

        // Reconstructing a context from the same root is one identity, and a
        // trailing separator normalizes to the same root.
        assert_eq!(
            alpha,
            LocalEndpointContext::with_unix_root(root_alpha).unwrap()
        );
        assert_eq!(
            alpha.namespace_id(),
            LocalEndpointContext::with_unix_root(root_alpha)
                .unwrap()
                .namespace_id()
        );
        assert_eq!(
            alpha,
            LocalEndpointContext::with_unix_root(Path::new("/tmp/c2-it-根A alpha/")).unwrap()
        );

        let mut contexts = std::collections::HashSet::new();
        contexts.insert(alpha.clone());
        assert!(!contexts.contains(&beta));
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_validation_rejects_invalid_paths() {
        use std::os::unix::ffi::OsStrExt;

        let cases: Vec<(std::ffi::OsString, &str)> = vec![
            (std::ffi::OsString::new(), "empty"),
            (std::ffi::OsString::from("relative/root"), "relative"),
            (std::ffi::OsString::from("/tmp/../escape"), "parent"),
            (std::ffi::OsString::from("/tmp/x/../y"), "parent"),
            (
                std::ffi::OsString::from(std::ffi::OsStr::from_bytes(b"/tmp/a\0b")),
                "nul",
            ),
            (
                std::ffi::OsString::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff")),
                "non-utf8",
            ),
        ];
        for (root, label) in cases {
            let error = LocalEndpointContext::with_unix_root(Path::new(&root)).expect_err(label);
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidInput,
                "{label}: {error}"
            );
        }

        // An interior "." alias is normalized, not rejected.
        let aliased = LocalEndpointContext::with_unix_root(Path::new("/tmp/./c2-it-b")).unwrap();
        assert_eq!(
            aliased,
            LocalEndpointContext::with_unix_root(Path::new("/tmp/c2-it-b")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn long_custom_root_is_the_final_directory_without_socket_capacity_checks() {
        let root = format!("/tmp/{}", "根目录 with spaces/".repeat(32));
        let context = LocalEndpointContext::with_unix_root(Path::new(&root)).unwrap();
        let endpoint = context.endpoint("ipc://Server-A").unwrap();
        let path = Path::new(endpoint.os_name());
        assert_eq!(path.parent(), context.unix_root());
        assert_eq!(
            path.file_name().unwrap(),
            "1118fcf083aa343ac0caf420a35a3025"
        );
        assert!(path.as_os_str().len() > 512);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_default_directory_has_the_default_identity() {
        let default = LocalEndpointContext::default_for_platform().unwrap();
        let explicit = LocalEndpointContext::with_unix_root(default.unix_root().unwrap()).unwrap();
        assert_eq!(explicit, default);
        assert_eq!(
            explicit.endpoint("ipc://same").unwrap(),
            default.endpoint("ipc://same").unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn frozen_context_is_stable_when_environment_changes() {
        use crate::{ConfigResolver, ConfigSources, EnvFilePolicy, LocalEndpointOptions};

        let first = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env_map(&[("C2_IPC_ROOT", "/tmp/c2-snapshot-a")]),
            },
        )
        .unwrap();
        let second = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env_map(&[("C2_IPC_ROOT", "/tmp/c2-snapshot-b")]),
            },
        )
        .unwrap();
        assert_ne!(first, second);

        let frozen = first.endpoint("ipc://snapshot").unwrap();
        assert!(
            frozen
                .os_name()
                .to_str()
                .unwrap()
                .starts_with("/tmp/c2-snapshot-a/")
        );
        // The already-frozen context keeps deriving its original names even
        // after a later resolution observed a different environment.
        assert_eq!(first.endpoint("ipc://snapshot").unwrap(), frozen);
        assert!(
            second
                .endpoint("ipc://snapshot")
                .unwrap()
                .os_name()
                .to_str()
                .unwrap()
                .starts_with("/tmp/c2-snapshot-b/")
        );
    }

    #[cfg(unix)]
    #[test]
    fn default_derivation_is_private_short_and_pure() {
        use sha2::{Digest, Sha256};

        // A per-run logical ID keeps the purity check independent of any real
        // deployed server name that might already own its derived socket path.
        let server_id = format!("slice-{}", uuid::Uuid::new_v4());
        let uid = unsafe { libc::geteuid() };
        let digest: String = Sha256::digest(server_id.as_bytes())[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let expected = format!("/tmp/c2-{uid:x}/{digest}");
        let expected_path = std::path::Path::new(&expected);
        let existed_before = expected_path.exists();
        assert!(!existed_before);

        let endpoint = LocalEndpoint::from_address(&format!("ipc://{server_id}")).unwrap();
        assert_eq!(endpoint.address(), format!("ipc://{server_id}"));
        assert_eq!(endpoint.os_name(), OsStr::new(&expected));
        assert_eq!(endpoint.protocol(), "managed-v2");
        assert_eq!(endpoint.namespace(), LocalEndpointNamespace::UnixFilesystem);
        assert_eq!(expected_path.file_name().unwrap().len(), 32);
        // Derivation is pure: it must not create the endpoint file.
        assert_eq!(expected_path.exists(), existed_before);

        let lower = LocalEndpoint::from_address("ipc://server-a").unwrap();
        let upper = LocalEndpoint::from_address("ipc://Server-A").unwrap();
        let unicode = LocalEndpoint::from_address("ipc://资源-Server-A").unwrap();
        assert_ne!(lower.os_name(), upper.os_name());
        assert_ne!(upper.os_name(), unicode.os_name());
    }

    #[cfg(unix)]
    #[test]
    fn long_logical_id_still_derives_a_bounded_native_name() {
        let address = format!("ipc://{}", "资源".repeat(100));
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        assert_eq!(Path::new(endpoint.os_name()).file_name().unwrap().len(), 32);
        assert_eq!(endpoint.address(), address);
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_name_is_bounded_and_has_no_nested_name_separators() {
        let endpoint =
            LocalEndpoint::from_address(&format!("ipc://{}", "资源".repeat(100))).unwrap();
        let name = endpoint.os_name().to_str().unwrap();
        assert!(name.starts_with(r"\\.\pipe\c_two-"));
        assert!(name.len() < 256);
        assert!(!name.strip_prefix(r"\\.\pipe\").unwrap().contains('\\'));
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_unix_root_override_as_not_applicable() {
        use crate::{ConfigResolver, ConfigSources, EnvFilePolicy, LocalEndpointOptions};

        let error = LocalEndpointContext::with_unix_root(Path::new("/tmp/c2-it-b")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("not applicable"), "{error}");

        let option_error = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions {
                unix_root: Some(PathBuf::from(r"C:\tmp\app")),
            },
            ConfigSources::empty(),
        )
        .unwrap_err();
        assert!(
            option_error.to_string().contains("not applicable"),
            "{option_error}"
        );

        let env_error = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources {
                env_file: EnvFilePolicy::Disabled,
                process_env: env_map(&[("C2_IPC_ROOT", r"C:\tmp\app")]),
            },
        )
        .unwrap_err();
        assert!(
            env_error.to_string().contains("not applicable"),
            "{env_error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_context_keeps_logon_pipe_identity() {
        let context = LocalEndpointContext::default_for_platform().unwrap();
        assert_eq!(
            context.platform_kind(),
            LocalEndpointNamespace::WindowsNamedPipe
        );
        assert_eq!(context.unix_root(), None);
        assert_eq!(context.unix_uid(), None);

        let scope = c2_local_security::current_scope_id().unwrap();
        assert_eq!(
            context.windows_logon_scope_id().as_deref(),
            Some(scope.as_str())
        );

        // The public identity is backed by the real logon scope; a fake root
        // or uid has no representation on this backend.
        let endpoint = context.endpoint("ipc://Server-A").unwrap();
        let expected = format!(r"\\.\pipe\c_two-{scope}-1118fcf083aa343ac0caf420a35a3025");
        assert_eq!(endpoint.os_name(), OsStr::new(&expected));
        assert_eq!(endpoint.protocol(), "named-pipe");
        assert_eq!(
            endpoint.namespace(),
            LocalEndpointNamespace::WindowsNamedPipe
        );
        assert_eq!(endpoint.context(), &context);
        assert_eq!(
            endpoint.context().namespace_id(),
            LocalEndpointContext::default_for_platform()
                .unwrap()
                .namespace_id()
        );
    }
}
