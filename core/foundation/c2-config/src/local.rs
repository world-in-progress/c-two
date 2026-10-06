//! Logical local addresses and their operating-system endpoint names.

use std::ffi::{OsStr, OsString};
use std::io;

/// Selects how a logical IPC address is projected into a local OS endpoint.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum LocalEndpointProtocol {
    /// The historical stable endpoint derivation.
    #[default]
    LegacyV1,
    /// A versioned private Unix socket namespace.
    ManagedV2,
}

impl std::str::FromStr for LocalEndpointProtocol {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "legacy-v1" => Ok(Self::LegacyV1),
            "managed-v2" => Ok(Self::ManagedV2),
            _ => Err(format!("unknown IPC endpoint protocol: {value}")),
        }
    }
}

impl LocalEndpointProtocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyV1 => "legacy-v1",
            Self::ManagedV2 => "managed-v2",
        }
    }
}

/// OS namespace used by an endpoint. The managed protocol remains a Unix
/// filesystem option; Windows retains its legacy named-pipe namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LocalEndpointNamespace {
    UnixFilesystem,
    WindowsNamedPipe,
}

/// A validated local IPC endpoint. The OS name is not a filesystem existence
/// probe: on Windows it names a pipe in the kernel namespace.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocalEndpoint {
    address: String,
    os_name: OsString,
    protocol: LocalEndpointProtocol,
    namespace: LocalEndpointNamespace,
}

impl LocalEndpoint {
    pub fn from_address(address: &str) -> io::Result<Self> {
        Self::from_address_with_protocol(address, LocalEndpointProtocol::LegacyV1)
    }

    pub fn from_address_with_protocol(
        address: &str,
        protocol: LocalEndpointProtocol,
    ) -> io::Result<Self> {
        let server_id = address.strip_prefix("ipc://").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid IPC address: {address}"),
            )
        })?;
        crate::validate_ipc_region_id(server_id)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(Self {
            address: address.to_owned(),
            os_name: endpoint_name(server_id, protocol)?,
            protocol,
            namespace: if cfg!(windows) {
                LocalEndpointNamespace::WindowsNamedPipe
            } else {
                LocalEndpointNamespace::UnixFilesystem
            },
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn os_name(&self) -> &OsStr {
        &self.os_name
    }

    pub const fn protocol(&self) -> LocalEndpointProtocol {
        self.protocol
    }

    pub const fn namespace(&self) -> LocalEndpointNamespace {
        self.namespace
    }

    pub fn transport_kind(&self) -> &'static str {
        if cfg!(windows) {
            "named-pipe"
        } else {
            "unix-socket"
        }
    }
}

#[cfg(unix)]
fn endpoint_name(server_id: &str, protocol: LocalEndpointProtocol) -> io::Result<OsString> {
    use std::os::unix::ffi::OsStrExt;

    let path = match protocol {
        LocalEndpointProtocol::LegacyV1 => {
            std::path::Path::new("/tmp/c_two_ipc").join(format!("{server_id}.sock"))
        }
        LocalEndpointProtocol::ManagedV2 => {
            use sha2::{Digest, Sha256};

            // The unreleased managed protocol's gate/record format 2 gets its
            // own rendezvous root. Never probe or adopt the older v2 root.
            let uid = unsafe { libc::geteuid() };
            let identity = format!("{:x}", Sha256::digest(server_id.as_bytes()));
            std::path::PathBuf::from(format!("/tmp/c2-{uid:x}/v2.2")).join(format!("{identity}.sock"))
        }
    };
    let os_name = path.into_os_string();
    let capacity = std::mem::size_of::<libc::sockaddr_un>() - offset_of_sun_path();
    if os_name.as_os_str().as_bytes().len() >= capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix socket endpoint exceeds sun_path capacity",
        ));
    }
    Ok(os_name)
}

#[cfg(unix)]
fn offset_of_sun_path() -> usize {
    std::mem::offset_of!(libc::sockaddr_un, sun_path)
}

#[cfg(windows)]
fn endpoint_name(server_id: &str, protocol: LocalEndpointProtocol) -> io::Result<OsString> {
    if protocol == LocalEndpointProtocol::ManagedV2 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed-v2 IPC endpoints are not supported on Windows",
        ));
    }
    use sha2::{Digest, Sha256};
    let scope = c2_local_security::current_scope_id()?;
    let identity = format!("{:x}", Sha256::digest(server_id.as_bytes()));
    Ok(format!(r"\\.\pipe\c_two-{scope}-{identity}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(unix)]
    #[test]
    fn managed_v2_derivation_is_private_versioned_bounded_and_pure() {
        use sha2::{Digest, Sha256};

        // A per-run logical ID keeps the purity check independent of any real
        // deployed server name that might already own its derived socket path.
        let server_id = format!("slice-{}", uuid::Uuid::new_v4());
        let uid = unsafe { libc::geteuid() };
        let digest = format!("{:x}", Sha256::digest(server_id.as_bytes()));
        let expected = format!("/tmp/c2-{uid:x}/v2.2/{digest}.sock");
        let expected_path = std::path::Path::new(&expected);
        let existed_before = expected_path.exists();
        assert!(!existed_before);

        let endpoint = LocalEndpoint::from_address_with_protocol(
            &format!("ipc://{server_id}"),
            LocalEndpointProtocol::ManagedV2,
        )
        .unwrap();
        assert_eq!(endpoint.address(), format!("ipc://{server_id}"));
        assert_eq!(endpoint.os_name(), OsStr::new(&expected));
        assert_eq!(endpoint.protocol(), LocalEndpointProtocol::ManagedV2);
        assert_eq!(endpoint.namespace(), LocalEndpointNamespace::UnixFilesystem);
        assert!(expected.len() < std::mem::size_of::<libc::sockaddr_un>() - offset_of_sun_path());
        // Derivation is pure: it must not create the endpoint file.
        assert_eq!(expected_path.exists(), existed_before);

        let lower = LocalEndpoint::from_address_with_protocol(
            "ipc://server-a",
            LocalEndpointProtocol::ManagedV2,
        )
        .unwrap();
        let upper = LocalEndpoint::from_address_with_protocol(
            "ipc://Server-A",
            LocalEndpointProtocol::ManagedV2,
        )
        .unwrap();
        let unicode = LocalEndpoint::from_address_with_protocol(
            "ipc://资源-Server-A",
            LocalEndpointProtocol::ManagedV2,
        )
        .unwrap();
        assert_ne!(lower.os_name(), upper.os_name());
        assert_ne!(upper.os_name(), unicode.os_name());
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_rejects_encoded_paths_over_sun_path_capacity() {
        let too_long = format!("ipc://{}", "a".repeat(100));
        assert_eq!(
            LocalEndpoint::from_address(&too_long).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[cfg(windows)]
    #[test]
    fn managed_v2_is_explicitly_unsupported_on_windows() {
        assert_eq!(
            LocalEndpoint::from_address_with_protocol(
                "ipc://server",
                LocalEndpointProtocol::ManagedV2,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Unsupported
        );
        assert!(
            LocalEndpoint::from_address("ipc://server")
                .unwrap()
                .os_name()
                .to_str()
                .unwrap()
                .starts_with(r"\\.\pipe\c_two-")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_keeps_the_canonical_socket_location() {
        assert_eq!(
            LocalEndpoint::from_address("ipc://unit-server")
                .unwrap()
                .os_name(),
            OsStr::new("/tmp/c_two_ipc/unit-server.sock")
        );
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
}
