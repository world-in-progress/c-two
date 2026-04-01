//! File-backed mmap spill and platform memory detection.
//!
//! Provides:
//! - [`available_physical_memory`] — query OS for free physical RAM
//! - [`should_spill`] — decide if an allocation should go to disk
//! - [`create_file_spill`] — create a file-backed mmap with unlink-on-create

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::MmapMut;

static SPILL_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Platform: macOS ────────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub fn available_physical_memory() -> u64 {
    use libc::{
        host_statistics64, mach_msg_type_number_t,
        sysconf, vm_statistics64, HOST_VM_INFO64,
        HOST_VM_INFO64_COUNT, _SC_PAGESIZE,
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

// ── Platform: Other ────────────────────────────────────────────────

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

/// Create a file-backed mmap buffer with unlink-on-create semantics.
///
/// The file is removed from the directory immediately after mmap, but
/// the mapping remains valid (POSIX guarantee). On process crash the
/// OS reclaims the fd + mmap — no residual files.
///
/// Returns `(mmap, path)` where `path` is for logging/debug only.
pub fn create_file_spill(
    size: usize,
    spill_dir: &Path,
) -> std::io::Result<(MmapMut, PathBuf)> {
    std::fs::create_dir_all(spill_dir)?;

    let pid = std::process::id();
    let seq = SPILL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let filename = format!("c2_{pid}_{seq}.spill");
    let path = spill_dir.join(&filename);

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.set_len(size as u64)?;

    // SAFETY: file is freshly created and exclusively owned.
    let mmap = unsafe { MmapMut::map_mut(&file)? };

    // Unlink-on-create: remove from directory, mmap stays valid.
    let _ = std::fs::remove_file(&path);

    Ok((mmap, path))
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_available_physical_memory_returns_nonzero() {
        let mem = available_physical_memory();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
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
}
