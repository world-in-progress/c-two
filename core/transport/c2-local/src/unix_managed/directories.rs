//! One socket namespace and its coordination records. Only the macOS default
//! namespace separates them; custom roots retain their application-owned layout.
use super::*;
use std::sync::Arc;

pub(super) type DirectoryEntries = std::iter::Flatten<
    std::iter::Chain<std::option::IntoIter<ReadDir>, std::option::IntoIter<ReadDir>>,
>;

#[derive(Clone, Debug)]
pub(super) struct NamespaceDirectories {
    pub(super) records: Arc<EndpointDirectory>,
    pub(super) sockets: Option<Arc<EndpointDirectory>>,
    root: PathBuf,
    default_root: Option<PathBuf>,
}

impl NamespaceDirectories {
    pub(super) fn open(root: &Path, create: bool) -> Result<Self, NamespaceError> {
        let default = LocalEndpointContext::default_for_platform().map_err(NamespaceError::Io)?;
        let default_root = default.unix_root().expect("Unix namespace");
        #[cfg(test)]
        let test_roots = TEST_ROOTS.with(|roots| roots.borrow().clone());
        #[cfg(test)]
        let default_root = test_roots
            .as_ref()
            .map_or(default_root, |roots| roots.0.as_path());
        let sockets = match EndpointDirectory::open(root, create && root == default_root) {
            Ok(directory) => Some(Arc::new(directory)),
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(directory_error(error, create)),
        };
        #[cfg(target_os = "macos")]
        let split =
            is_default(root, sockets.as_deref(), default_root).map_err(NamespaceError::Io)?;
        #[cfg(not(target_os = "macos"))]
        let split = false;
        let records = if split {
            #[cfg(target_os = "macos")]
            let records_root = coordination_root().map_err(NamespaceError::Io)?;
            #[cfg(not(target_os = "macos"))]
            let records_root = root.to_owned();
            #[cfg(test)]
            let records_root = test_roots
                .as_ref()
                .map_or(records_root, |roots| roots.1.clone());
            match EndpointDirectory::open(&records_root, create) {
                Ok(directory) => Arc::new(directory),
                Err(error) if !create && error.kind() == io::ErrorKind::NotFound => {
                    return if sockets.is_some() && has_endpoint_entries(fs::read_dir(root)?)? {
                        Err(NamespaceError::Unverified(
                            EndpointUnverifiedReason::InitializationIncomplete,
                        ))
                    } else {
                        Err(NamespaceError::Absent)
                    };
                }
                Err(error) => return Err(NamespaceError::Io(error)),
            }
        } else {
            sockets.clone().ok_or(NamespaceError::Absent)?
        };
        let directories = Self {
            records,
            sockets,
            root: root.to_owned(),
            default_root: split.then(|| default_root.to_owned()),
        };
        directories.validate().map_err(NamespaceError::Io)?;
        Ok(directories)
    }

    pub(super) fn split(&self) -> bool {
        self.default_root.is_some()
    }

    pub(super) fn validate(&self) -> io::Result<()> {
        self.records.validate()?;
        if let Some(sockets) = &self.sockets {
            sockets.validate()?;
        } else {
            match fs::symlink_metadata(&self.root) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => return Err(io::Error::other("socket namespace changed while absent")),
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(default_root) = &self.default_root {
            if !is_default(&self.root, self.sockets.as_deref(), default_root)? {
                return Err(io::Error::other(
                    "default socket namespace identity changed",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn socket_stat(&self, name: &CString) -> io::Result<Option<libc::stat>> {
        self.sockets
            .as_ref()
            .map_or(Ok(None), |directory| directory.stat(name))
    }

    pub(super) fn socket_directory(&self) -> io::Result<&EndpointDirectory> {
        self.sockets
            .as_deref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "socket namespace is absent"))
    }

    pub(super) fn entries(&self) -> io::Result<DirectoryEntries> {
        let sockets = self
            .sockets
            .as_ref()
            .map(|_| fs::read_dir(&self.root))
            .transpose()?;
        // The directory's original path is kept by EndpointDirectory. Resolve
        // the records path only at open; traversal must use that same name.
        let records = if self.split() {
            Some(fs::read_dir(self.records.path())?)
        } else {
            None
        };
        Ok(sockets.into_iter().chain(records).flatten())
    }

    pub(super) fn same_as(&self, other: &Self) -> io::Result<bool> {
        if self.validate().is_err() || other.validate().is_err() {
            return Ok(false);
        }
        Ok(
            same_file(&fstat(self.records.fd())?, &fstat(other.records.fd())?)
                && match (&self.sockets, &other.sockets) {
                    (Some(a), Some(b)) => same_file(&fstat(a.fd())?, &fstat(b.fd())?),
                    (None, None) => true,
                    _ => false,
                },
        )
    }
}

#[cfg(target_os = "macos")]
fn is_default(
    root: &Path,
    sockets: Option<&EndpointDirectory>,
    default: &Path,
) -> io::Result<bool> {
    match (sockets, fs::symlink_metadata(default)) {
        (Some(sockets), Ok(metadata)) => {
            use std::os::unix::fs::MetadataExt;
            let opened = fstat(sockets.fd())?;
            Ok(metadata.is_dir()
                && metadata.dev() == opened.st_dev as u64
                && metadata.ino() == opened.st_ino as u64)
        }
        (_, Err(error)) if error.kind() != io::ErrorKind::NotFound => Err(error),
        (None, Err(_)) => {
            // The default leaf can disappear at reboot. Its system-owned parent
            // remains resolvable, so aliases still select the one coordinator.
            fn slot(path: &Path) -> io::Result<PathBuf> {
                Ok(fs::canonicalize(
                    path.parent()
                        .ok_or_else(|| io::Error::other("missing parent"))?,
                )?
                .join(
                    path.file_name()
                        .ok_or_else(|| io::Error::other("missing leaf"))?,
                ))
            }
            let root_slot = match slot(root) {
                Ok(slot) => slot,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            Ok(root_slot == slot(default)?)
        }
        _ => Ok(false),
    }
}

#[cfg(target_os = "macos")]
fn coordination_root() -> io::Result<PathBuf> {
    use std::ffi::CStr;
    let size = unsafe { libc::confstr(libc::_CS_DARWIN_USER_DIR, std::ptr::null_mut(), 0) };
    if size == 0 {
        return Err(io::Error::other("cannot resolve Darwin user directory"));
    }
    let mut path = vec![0_u8; size];
    let actual = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_DIR,
            path.as_mut_ptr().cast(),
            path.len(),
        )
    };
    if actual == 0 || actual > path.len() {
        return Err(io::Error::other("cannot resolve Darwin user directory"));
    }
    let path = CStr::from_bytes_with_nul(&path[..actual])
        .map_err(|_| io::Error::other("invalid Darwin user directory"))?;
    Ok(Path::new(OsStr::from_bytes(path.to_bytes())).join(format!(
        "c2-ipc-coordination-v{}",
        c2_config::UNIX_NAMESPACE_FORMAT_VERSION
    )))
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_ROOTS: std::cell::RefCell<Option<(PathBuf, PathBuf)>> = const { std::cell::RefCell::new(None) };
}
