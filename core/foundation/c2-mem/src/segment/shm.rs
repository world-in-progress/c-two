//! Platform-owned shared memory mappings with one public lifecycle contract.

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

pub use platform::ShmRegion;
