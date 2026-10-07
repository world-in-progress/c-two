//! Logical local addresses and their operating-system endpoint names.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io;

/// Native OS namespace used by an endpoint.
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
    namespace: LocalEndpointNamespace,
}

impl LocalEndpoint {
    pub fn from_address(address: &str) -> io::Result<Self> {
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
            os_name: endpoint_name(server_id)?,
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

    /// Strict credential metadata for the platform's native backend.
    pub const fn protocol(&self) -> &'static str {
        match self.namespace {
            LocalEndpointNamespace::UnixFilesystem => "managed-v2",
            LocalEndpointNamespace::WindowsNamedPipe => "named-pipe",
        }
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

fn endpoint_identity(server_id: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut identity = String::with_capacity(64);
    for byte in Sha256::digest(server_id.as_bytes()) {
        write!(&mut identity, "{byte:02x}").expect("writing to String cannot fail");
    }
    identity
}

#[cfg(unix)]
fn endpoint_name(server_id: &str) -> io::Result<OsString> {
    use std::os::unix::ffi::OsStrExt;

    let uid = unsafe { libc::geteuid() };
    let identity = endpoint_identity(server_id);
    let path =
        std::path::PathBuf::from(format!("/tmp/c2-{uid:x}/v2.2")).join(format!("{identity}.sock"));
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
fn endpoint_name(server_id: &str) -> io::Result<OsString> {
    let scope = c2_local_security::current_scope_id()?;
    let identity = endpoint_identity(server_id);
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

    #[test]
    fn endpoint_names_preserve_sha256_hex_golden_values() {
        for (server_id, digest) in [
            (
                "Server-A",
                "1118fcf083aa343ac0caf420a35a30251f311befab9f337abdf921977668e0cd",
            ),
            (
                "server-a",
                "a79b8498a1fb0114738f243cfd7c1eeae3d52e888e79000e31cb6f0bf2c077eb",
            ),
            (
                "资源-Server-A",
                "4ad7b4ee929ad2c148664fcaa37c16e8fd17455d6cbb20f33fc14e9313a23e36",
            ),
        ] {
            let endpoint = LocalEndpoint::from_address(&format!("ipc://{server_id}")).unwrap();
            #[cfg(unix)]
            let expected = format!("/tmp/c2-{:x}/v2.2/{digest}.sock", unsafe {
                libc::geteuid()
            });
            #[cfg(windows)]
            let expected = format!(
                r"\\.\pipe\c_two-{}-{digest}",
                c2_local_security::current_scope_id().unwrap()
            );
            assert_eq!(endpoint.os_name(), OsStr::new(&expected));
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_v2_derivation_is_private_versioned_bounded_and_pure() {
        use sha2::{Digest, Sha256};

        // A per-run logical ID keeps the purity check independent of any real
        // deployed server name that might already own its derived socket path.
        let server_id = format!("slice-{}", uuid::Uuid::new_v4());
        let uid = unsafe { libc::geteuid() };
        let digest: String = Sha256::digest(server_id.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let expected = format!("/tmp/c2-{uid:x}/v2.2/{digest}.sock");
        let expected_path = std::path::Path::new(&expected);
        let existed_before = expected_path.exists();
        assert!(!existed_before);

        let endpoint = LocalEndpoint::from_address(&format!("ipc://{server_id}")).unwrap();
        assert_eq!(endpoint.address(), format!("ipc://{server_id}"));
        assert_eq!(endpoint.os_name(), OsStr::new(&expected));
        assert_eq!(endpoint.protocol(), "managed-v2");
        assert_eq!(endpoint.namespace(), LocalEndpointNamespace::UnixFilesystem);
        assert!(expected.len() < std::mem::size_of::<libc::sockaddr_un>() - offset_of_sun_path());
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
        use std::os::unix::ffi::OsStrExt;
        assert!(
            endpoint.os_name().as_bytes().len()
                < std::mem::size_of::<libc::sockaddr_un>() - offset_of_sun_path()
        );
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
}
