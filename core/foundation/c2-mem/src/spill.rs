//! File-backed mmap spill and platform memory detection.
//!
//! Provides:
//! - [`available_physical_memory`] — query OS for free physical RAM
//! - [`should_spill`] — decide if an allocation should go to disk
//! - [`create_file_spill`] — create a private mapping with OS-owned cleanup

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::MmapMut;

/// The mapping closes before its backing file. On Windows the file carries
/// DELETE_ON_CLOSE, so normal drop and process termination both remove it.
#[derive(Debug)]
pub struct SpillMapping {
    mapping: MmapMut,
    _file: std::fs::File,
}

impl std::ops::Deref for SpillMapping {
    type Target = MmapMut;

    fn deref(&self) -> &Self::Target {
        &self.mapping
    }
}

impl std::ops::DerefMut for SpillMapping {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.mapping
    }
}

static SPILL_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Platform: macOS ────────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub fn available_physical_memory() -> u64 {
    use libc::{
        _SC_PAGESIZE, HOST_VM_INFO64, HOST_VM_INFO64_COUNT, host_statistics64,
        mach_msg_type_number_t, sysconf, vm_statistics64,
    };
    unsafe {
        let page_size = sysconf(_SC_PAGESIZE) as u64;
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        let mut vm_stat: vm_statistics64 = std::mem::zeroed();
        let mut count: mach_msg_type_number_t = HOST_VM_INFO64_COUNT as _;
        let kr = host_statistics64(
            host,
            HOST_VM_INFO64 as _,
            &mut vm_stat as *mut _ as *mut _,
            &mut count,
        );
        if kr != 0 {
            return 0; // fallback: report 0 → always spill
        }
        let free = vm_stat.free_count as u64;
        let inactive = vm_stat.inactive_count as u64;
        (free + inactive) * page_size
    }
}

// ── Platform: Linux ────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub fn available_physical_memory() -> u64 {
    if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let trimmed = rest.trim().trim_end_matches(" kB").trim();
                if let Ok(kb) = trimmed.parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0 // fallback: always spill
}

// Windows paging-file-backed mappings consume commit as well as RAM.
#[cfg(windows)]
pub fn available_physical_memory() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return 0;
    }
    status.ullAvailPhys.min(status.ullAvailPageFile)
}

// ── Platform: Other ────────────────────────────────────────────────

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn available_physical_memory() -> u64 {
    0 // conservative: always spill on unknown platforms
}

// ── Spill decision ─────────────────────────────────────────────────

/// Returns `true` when the requested allocation should use file-backed
/// mmap instead of shared memory.
///
/// The heuristic: if `requested > available_ram * threshold`, spill.
/// A threshold of 0.0 forces all allocations to spill (useful for tests).
/// A threshold of 1.0 effectively disables spilling.
pub fn should_spill(requested: usize, threshold: f64) -> bool {
    if threshold <= 0.0 {
        return true;
    }
    if threshold >= 1.0 {
        return false;
    }
    let available = available_physical_memory();
    requested as u64 > (available as f64 * threshold) as u64
}

// ── File-backed mmap ───────────────────────────────────────────────

/// Create a private file-backed mapping. Unix unlinks the open file;
/// Windows deletes it when the owned mapping and file handle close.
///
/// Returns `(mmap, path)` where `path` is for logging/debug only.
pub fn create_file_spill(
    size: usize,
    spill_dir: &Path,
) -> std::io::Result<(SpillMapping, PathBuf)> {
    if size == 0 || size > isize::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid spill size",
        ));
    }
    std::fs::create_dir_all(spill_dir)?;

    let pid = std::process::id();
    let seq = SPILL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let filename = format!("c2_{pid}_{seq}.spill");
    let path = spill_dir.join(&filename);

    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_DELETE_ON_CLOSE,
        };
        options
            .share_mode(0)
            .custom_flags(FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE);
    }
    let file = options.open(&path)?;
    // Unlink before fallible sizing/mapping on Unix, so errors also clean up.
    #[cfg(unix)]
    std::fs::remove_file(&path)?;
    file.set_len(size as u64)?;

    // SAFETY: file is freshly created and exclusively owned.
    let mmap = unsafe { MmapMut::map_mut(&file)? };

    Ok((
        SpillMapping {
            mapping: mmap,
            _file: file,
        },
        path,
    ))
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_available_physical_memory_returns_nonzero() {
        let mem = available_physical_memory();
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        assert!(mem > 0, "expected nonzero available memory, got {mem}");
    }

    #[test]
    fn test_should_spill_threshold_zero_always_spills() {
        assert!(should_spill(1, 0.0));
    }

    #[test]
    fn test_should_spill_threshold_one_never_spills() {
        assert!(!should_spill(usize::MAX, 1.0));
    }

    #[test]
    fn test_should_spill_small_allocation_does_not_spill() {
        assert!(!should_spill(1, 0.8));
    }

    #[test]
    fn test_create_file_spill_and_readback() {
        let dir = std::env::temp_dir().join("c2_spill_test");
        let _ = std::fs::remove_dir_all(&dir);

        let (mut mmap, _path) = create_file_spill(4096, &dir).unwrap();

        let pattern = b"hello_spill";
        mmap[..pattern.len()].copy_from_slice(pattern);
        mmap.flush().unwrap();
        assert_eq!(&mmap[..pattern.len()], pattern);

        // Windows marks the file for deletion while the mapping is live.
        // The last owned view/handle release completes that deletion.
        drop(mmap);

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "spill file should be unlinked");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_file_spill_release() {
        let dir = std::env::temp_dir().join("c2_spill_release_test");
        let _ = std::fs::remove_dir_all(&dir);

        let (mmap, _path) = create_file_spill(8192, &dir).unwrap();
        drop(mmap);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_process_child() {
        let Some(directory) = std::env::var_os("C2_TEST_SPILL_CHILD_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let (mut mapping, _) = create_file_spill(8192, &directory).unwrap();
        mapping[..4].copy_from_slice(b"live");
        std::fs::write(directory.join("ready"), b"ready").unwrap();
        // Parent intentionally kills this process while its mapping is live.
        std::thread::sleep(std::time::Duration::from_secs(30));
        std::hint::black_box(mapping);
    }

    #[test]
    fn spill_is_removed_when_owner_process_is_killed() {
        let directory = std::env::temp_dir().join(format!("c2_spill_crash_{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let ready = directory.join("ready");
        let _ = std::fs::remove_file(&ready);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "spill::tests::spill_process_child",
                "--nocapture",
            ])
            .env("C2_TEST_SPILL_CHILD_DIR", &directory)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() && std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let was_ready = ready.exists();
        let _ = child.kill();
        child.wait().unwrap();
        let remaining: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "spill")
            })
            .collect();
        let _ = std::fs::remove_file(&ready);
        let _ = std::fs::remove_dir(&directory);
        assert!(was_ready, "child did not create its live spill mapping");
        assert!(
            remaining.is_empty(),
            "spill files survive killed owner: {remaining:?}"
        );
    }
}
