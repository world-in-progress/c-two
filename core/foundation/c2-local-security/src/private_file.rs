//! Creation and validation of owner-only persistent files.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

/// Create the final directory with owner-only access, or verify an existing one.
/// The caller supplies a trusted parent directory.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let result = fs::DirBuilder::new().mode(0o700).create(path);
    #[cfg(windows)]
    let result = crate::windows::create_owner_directory(path);
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            verify_private_path(path, true)
        }
        Err(error) => Err(error),
    }
}

/// Create and sync a new owner-only file. Existing entries are never replaced.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = open_new_private_file(path)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// Check an existing private regular file before reusing it.
pub fn verify_private_file(path: &Path) -> io::Result<()> {
    verify_private_path(path, false)
}

fn verify_private_path(path: &Path, directory: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a private directory or regular file",
        ));
    }
    #[cfg(unix)]
    {
        let mode = if directory { 0o700 } else { 0o600 };
        // SAFETY: geteuid takes no pointers and has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != mode {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("expected current user ownership and mode {mode:04o}"),
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private path is a reparse point",
            ));
        }
        crate::windows::verify_owner_path(path)
    }
}

fn open_new_private_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    return fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path);
    #[cfg(windows)]
    return crate::windows::create_owner_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_creation_and_reopen() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("private");
        create_private_dir(&dir).unwrap();
        create_private_dir(&dir).unwrap();
        let key = dir.join("key");
        write_private_file(&key, b"first").unwrap();
        verify_private_file(&key).unwrap();
        assert_eq!(
            write_private_file(&key, b"replacement").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&key).unwrap(), b"first");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&key).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                verify_private_file(&key).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }
}
