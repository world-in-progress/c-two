use crate::{CodegenError, ContractArtifactSet, lower_hex};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

impl ContractArtifactSet {
    /// Atomically makes this complete regular-file set visible at an absent destination.
    ///
    /// The destination parent must already exist and remain stable. This refuses updates and
    /// destination symlinks. It guarantees no-replace atomic visibility, not directory fsync
    /// durability across power loss or protection from an adversary replacing parent components.
    pub fn publish_new_tree(&self, destination: &Path) -> Result<(), CodegenError> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if destination.file_name().is_none() {
            return Err(CodegenError::InvalidDestination {
                path: destination.to_path_buf(),
            });
        }
        let parent_metadata = fs::metadata(parent)
            .map_err(|source| io_error("reading destination parent", parent, source))?;
        if !parent_metadata.is_dir() {
            return Err(CodegenError::InvalidDestination {
                path: destination.to_path_buf(),
            });
        }
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                return Err(CodegenError::DestinationExists {
                    path: destination.to_path_buf(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(io_error("checking destination", destination, source));
            }
        }
        ensure_atomic_publication_supported()?;

        let staging = tempfile::Builder::new()
            .prefix(".c-two-stage-")
            .tempdir_in(parent)
            .map_err(|source| io_error("creating staging directory", parent, source))?;
        for artifact in self.artifacts() {
            let output = staging.path().join(artifact.relative_path());
            create_parent_directories(staging.path(), &output)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .map_err(|source| io_error("creating artifact", &output, source))?;
            file.write_all(artifact.bytes())
                .map_err(|source| io_error("writing artifact", &output, source))?;
            file.sync_all()
                .map_err(|source| io_error("syncing artifact", &output, source))?;
            drop(file);

            let written_hash = sha256_file(&output)?;
            if written_hash != *artifact.sha256() {
                return Err(CodegenError::ArtifactHashMismatch {
                    relative_path: artifact.relative_path().to_string(),
                    expected: artifact.sha256_hex(),
                    actual: lower_hex(&written_hash),
                });
            }
        }

        rename_directory_noreplace(staging.path(), destination)?;
        let _staging_path = staging.keep();
        Ok(())
    }
}

fn create_parent_directories(staging_root: &Path, output: &Path) -> Result<(), CodegenError> {
    let parent = output
        .parent()
        .ok_or_else(|| CodegenError::InvalidDestination {
            path: output.to_path_buf(),
        })?;
    let relative =
        parent
            .strip_prefix(staging_root)
            .map_err(|_| CodegenError::InvalidDestination {
                path: output.to_path_buf(),
            })?;
    let mut current = staging_root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|source| io_error("checking artifact directory", &current, source))?;
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(CodegenError::InvalidDestination { path: current });
                }
            }
            Err(source) => {
                return Err(io_error("creating artifact directory", &current, source));
            }
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<[u8; 32], CodegenError> {
    let mut file =
        File::open(path).map_err(|source| io_error("reopening artifact", path, source))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| io_error("reading artifact", path, source))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
    windows,
))]
fn ensure_atomic_publication_supported() -> Result<(), CodegenError> {
    Ok(())
}

#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
    windows,
)))]
fn ensure_atomic_publication_supported() -> Result<(), CodegenError> {
    Err(CodegenError::AtomicPublicationUnsupported)
}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
))]
fn rename_directory_noreplace(source: &Path, destination: &Path) -> Result<(), CodegenError> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;

    renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE).map_err(|error| {
        if matches!(error, Errno::NOSYS | Errno::INVAL | Errno::NOTSUP) {
            return CodegenError::AtomicPublicationUnsupported;
        }
        let source = std::io::Error::from_raw_os_error(error.raw_os_error());
        if source.kind() == std::io::ErrorKind::AlreadyExists {
            CodegenError::DestinationExists {
                path: destination.to_path_buf(),
            }
        } else {
            io_error("publishing artifact tree", destination, source)
        }
    })
}

#[cfg(windows)]
fn rename_directory_noreplace(source: &Path, destination: &Path) -> Result<(), CodegenError> {
    fs::rename(source, destination).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AlreadyExists || destination.exists() {
            CodegenError::DestinationExists {
                path: destination.to_path_buf(),
            }
        } else {
            io_error("publishing artifact tree", destination, source)
        }
    })
}

#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
    windows,
)))]
fn rename_directory_noreplace(_source: &Path, _destination: &Path) -> Result<(), CodegenError> {
    Err(CodegenError::AtomicPublicationUnsupported)
}

fn io_error(
    operation: &'static str,
    path: impl Into<PathBuf>,
    source: std::io::Error,
) -> CodegenError {
    CodegenError::Io {
        operation,
        path: path.into(),
        source,
    }
}
