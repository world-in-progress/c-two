//! Headerless dedicated SHM for one locally owned allocation.

use crate::budget::BudgetReservation;
use crate::segment::ChargedMapping;
use crate::segment::ShmRegion;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
pub(crate) fn native_page_size() -> Option<usize> {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).ok().filter(|size| *size > 0)
}

#[cfg(windows)]
pub(crate) fn native_page_size() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    usize::try_from(info.dwPageSize)
        .ok()
        .filter(|size| *size > 0)
}

fn round_payload_to_page(data_size: usize, page_size: usize) -> Option<usize> {
    if data_size == 0 {
        return None;
    }
    let remainder = data_size % page_size;
    let padded = if remainder == 0 {
        data_size
    } else {
        data_size.checked_add(page_size - remainder)?
    };
    (padded <= isize::MAX as usize).then_some(padded)
}

pub struct DedicatedSegment {
    name: String,
    size: usize,
    mapping: Mutex<Option<Arc<ChargedMapping>>>,
}

impl DedicatedSegment {
    /// Page-rounded payload capacity, bounded by one Rust addressable span.
    pub fn required_shm_size(data_size: usize) -> Option<usize> {
        round_payload_to_page(data_size, native_page_size()?)
    }

    pub fn create(name: &str, data_size: usize) -> Result<Self, String> {
        let size = Self::required_shm_size(data_size).ok_or_else(|| {
            format!("dedicated backing geometry unsupported for {data_size} payload bytes")
        })?;
        Ok(Self::from_region(ShmRegion::create(name, size)?))
    }

    /// Open the OS object's full, read-only view; no geometry lives in SHM.
    pub fn open(name: &str, min_size: usize) -> Result<Self, String> {
        Ok(Self::from_region(ShmRegion::open(name, min_size)?))
    }

    fn from_region(region: ShmRegion) -> Self {
        Self {
            name: region.name().to_owned(),
            size: region.size(),
            mapping: Mutex::new(Some(Arc::new(ChargedMapping {
                region,
                reservation: None,
            }))),
        }
    }

    pub(crate) fn with_budget_guard(mut self, guard: BudgetReservation) -> Self {
        Arc::get_mut(
            self.mapping
                .get_mut()
                .expect("dedicated mapping poisoned")
                .as_mut()
                .unwrap(),
        )
        .expect("backing not published")
        .reservation = Some(guard);
        self
    }

    pub(crate) fn pin(&self) -> Arc<ChargedMapping> {
        self.try_pin().expect("live allocation pins its mapping")
    }

    pub(crate) fn try_pin(&self) -> Option<Arc<ChargedMapping>> {
        self.mapping
            .lock()
            .expect("dedicated mapping poisoned")
            .clone()
    }

    pub(crate) fn is_mapped(&self) -> bool {
        self.mapping
            .lock()
            .expect("dedicated mapping poisoned")
            .is_some()
    }

    pub(crate) fn release(&self) {
        let mapping = self
            .mapping
            .lock()
            .expect("dedicated mapping poisoned")
            .take();
        drop(mapping);
    }

    /// Raw data at offset zero. A peer mapping may only be read.
    pub fn data_ptr(&self) -> *mut u8 {
        self.pin().region.base_ptr()
    }
    pub fn data_size(&self) -> usize {
        self.size
    }
    pub fn size(&self) -> usize {
        self.size
    }
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headerless_payload_and_full_peer_span() {
        let name = format!("/c2ded_{}", std::process::id());
        let span = DedicatedSegment::required_shm_size(8192).unwrap();
        let page = native_page_size().unwrap();
        let creator = DedicatedSegment::create(&name, 8192).unwrap();
        assert_eq!(creator.size(), span);
        assert_eq!(creator.data_ptr(), creator.pin().region.base_ptr());
        unsafe {
            *creator.data_ptr() = 0xAB;
            if span > page {
                *creator.data_ptr().add(span - page) = 0xBC;
            }
            *creator.data_ptr().add(span - 1) = 0xCD;
        }
        let reader = DedicatedSegment::open(&name, 1).unwrap();
        assert_eq!(reader.data_size(), span);
        assert_eq!(unsafe { *reader.data_ptr() }, 0xAB);
        if span > page {
            assert_eq!(unsafe { *reader.data_ptr().add(span - page) }, 0xBC);
        }
        assert_eq!(unsafe { *reader.data_ptr().add(span - 1) }, 0xCD);
        assert!(DedicatedSegment::create(&name, 4096).is_err());
        assert_eq!(unsafe { *reader.data_ptr() }, 0xAB);
    }

    #[test]
    fn page_rounding_and_overflow_before_mapping() {
        let page = native_page_size().expect("OS must report a page size");
        assert_eq!(DedicatedSegment::required_shm_size(0), None);
        assert_eq!(DedicatedSegment::required_shm_size(1), Some(page));
        assert_eq!(DedicatedSegment::required_shm_size(page - 1), Some(page));
        assert_eq!(DedicatedSegment::required_shm_size(page), Some(page));
        assert_eq!(
            DedicatedSegment::required_shm_size(page + 1),
            Some(2 * page)
        );
        assert_eq!(DedicatedSegment::required_shm_size(usize::MAX), None);
        assert_eq!(
            DedicatedSegment::required_shm_size(isize::MAX as usize),
            None
        );
    }

    #[test]
    fn sixteen_kib_page_rounding_charges_full_mapping() {
        const PAGE: usize = 16 * 1024;
        assert_eq!(round_payload_to_page(1, PAGE), Some(PAGE));
        assert_eq!(round_payload_to_page(PAGE - 1, PAGE), Some(PAGE));
        assert_eq!(round_payload_to_page(PAGE, PAGE), Some(PAGE));
        assert_eq!(round_payload_to_page(PAGE + 1, PAGE), Some(2 * PAGE));
        assert_eq!(round_payload_to_page(8192, PAGE), Some(PAGE));
        assert_eq!(round_payload_to_page(0, PAGE), None);
        assert_eq!(round_payload_to_page(usize::MAX, PAGE), None);
        assert_eq!(round_payload_to_page(isize::MAX as usize, PAGE), None);
    }
}
