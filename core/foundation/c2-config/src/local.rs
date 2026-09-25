//! Logical local addresses and their operating-system endpoint names.

use std::ffi::{OsStr, OsString};
use std::io;

/// A validated local IPC endpoint. The OS name is not a filesystem existence
/// probe: on Windows it names a pipe in the kernel namespace.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocalEndpoint {
    address: String,
    os_name: OsString,
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
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn os_name(&self) -> &OsStr {
        &self.os_name
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
fn endpoint_name(server_id: &str) -> io::Result<OsString> {
    Ok(std::path::Path::new("/tmp/c_two_ipc")
        .join(format!("{server_id}.sock"))
        .into_os_string())
}

#[cfg(windows)]
fn endpoint_name(server_id: &str) -> io::Result<OsString> {
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
