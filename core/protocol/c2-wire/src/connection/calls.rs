//! Physical-connection call identity and one terminal outcome per original call.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::rpc::CallStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CallPhase {
    PreparingInput,
    Queued,
    Executing,
    Ending,
}

pub struct CallRecord<T> {
    phase: AtomicU8,
    pub data: T,
}

impl<T> CallRecord<T> {
    pub fn queue(&self) -> bool {
        self.phase
            .compare_exchange(
                CallPhase::PreparingInput as u8,
                CallPhase::Queued as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
    pub fn enter_resource(&self) -> bool {
        self.phase
            .compare_exchange(
                CallPhase::Queued as u8,
                CallPhase::Executing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
    pub fn is_preparing_input(&self) -> bool {
        self.phase.load(Ordering::Acquire) == CallPhase::PreparingInput as u8
    }
    /// Claim a refusal only while entering the resource is still impossible.
    pub fn reject_before_execution(&self) -> Option<CallStatus> {
        let mut phase = self.phase.load(Ordering::Acquire);
        loop {
            if phase != CallPhase::PreparingInput as u8 && phase != CallPhase::Queued as u8 {
                return None;
            }
            match self.phase.compare_exchange_weak(
                phase,
                CallPhase::Ending as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(CallStatus::NotExecuted),
                Err(current) => phase = current,
            }
        }
    }

    /// Only the resource completion path can finish an executing call.
    pub fn finish_execution(&self, success: bool) -> Option<CallStatus> {
        self.phase
            .compare_exchange(
                CallPhase::Executing as u8,
                CallPhase::Ending as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| {
                if success {
                    CallStatus::Success
                } else {
                    CallStatus::Failed
                }
            })
    }
}

pub enum CallLookup<T> {
    Active(Arc<CallRecord<T>>),
    Retired,
    Future,
}

pub struct CallTable<T> {
    high_open: u64,
    active: HashMap<u64, Arc<CallRecord<T>>>,
}
impl<T> Default for CallTable<T> {
    fn default() -> Self {
        Self {
            high_open: 0,
            active: HashMap::new(),
        }
    }
}
impl<T> CallTable<T> {
    /// Advance the request-ID high-water mark for a new OPEN without
    /// creating an active record. A pre-admission refusal still advances the
    /// mark so late continuations of the refused ID land on Retired and are
    /// dropped cleanly.
    pub fn advance_open(&mut self, id: u64) -> Result<(), &'static str> {
        if id == 0 || id <= self.high_open {
            return Err("OPEN request ID is not increasing");
        }
        self.high_open = id;
        Ok(())
    }

    /// Insert the active record for an ID whose high-water mark was already
    /// advanced (id == high_open, not already active).
    pub fn insert_open(&mut self, id: u64, data: T) -> Result<Arc<CallRecord<T>>, &'static str> {
        if id == 0 || id != self.high_open {
            return Err("OPEN request ID is not increasing");
        }
        if self.active.contains_key(&id) {
            return Err("OPEN request ID already active");
        }
        let call = Arc::new(CallRecord {
            phase: AtomicU8::new(CallPhase::PreparingInput as u8),
            data,
        });
        self.active.insert(id, call.clone());
        Ok(call)
    }

    /// Combined advance and insert for callers that admit an OPEN in one step.
    pub fn open(&mut self, id: u64, data: T) -> Result<Arc<CallRecord<T>>, &'static str> {
        self.advance_open(id)?;
        self.insert_open(id, data)
    }
    pub fn get(&self, id: u64) -> Option<Arc<CallRecord<T>>> {
        self.active.get(&id).cloned()
    }
    pub fn continuation(&self, id: u64) -> CallLookup<T> {
        if let Some(call) = self.get(id) {
            CallLookup::Active(call)
        } else if id != 0 && id <= self.high_open {
            CallLookup::Retired
        } else {
            CallLookup::Future
        }
    }
    pub fn retire(&mut self, id: u64) -> Option<Arc<CallRecord<T>>> {
        self.active.remove(&id)
    }
    pub fn active(&self) -> impl Iterator<Item = (u64, Arc<CallRecord<T>>)> + '_ {
        self.active.iter().map(|(id, call)| (*id, call.clone()))
    }
}

/// The receive loop owns the one cumulative position; workers receive indices.
pub struct ValueReceive {
    total_bytes: u64,
    part_bytes: u64,
    admitted_bytes: u64,
}
impl ValueReceive {
    pub fn first(total_bytes: u64, first_bytes: u64) -> Result<Self, &'static str> {
        if first_bytes == 0 || first_bytes >= total_bytes {
            return Err("invalid chunked value length");
        }
        Ok(Self {
            total_bytes,
            part_bytes: first_bytes,
            admitted_bytes: 0,
        })
    }
    pub fn admit(&mut self, bytes: u64) -> Result<usize, &'static str> {
        let remaining = self.total_bytes - self.admitted_bytes;
        if remaining == 0 || bytes != self.part_bytes.min(remaining) {
            return Err("value continuation length does not match remaining bytes");
        }
        let index = usize::try_from(self.admitted_bytes / self.part_bytes)
            .map_err(|_| "value position is not addressable")?;
        self.admitted_bytes += bytes;
        Ok(index)
    }
    pub fn geometry(&self) -> Result<(usize, usize), &'static str> {
        Ok((
            usize::try_from(self.total_bytes.div_ceil(self.part_bytes))
                .map_err(|_| "value count is not addressable")?,
            usize::try_from(self.part_bytes).map_err(|_| "value part is not addressable")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn high_water_outlives_storage_and_refusal() {
        let mut table = CallTable::default();
        let old = table.open(7, ()).unwrap();
        assert_eq!(old.reject_before_execution(), Some(CallStatus::NotExecuted));
        table.retire(7);
        assert!(matches!(table.continuation(7), CallLookup::Retired));
        assert!(matches!(table.continuation(8), CallLookup::Future));
        assert!(table.open(7, ()).is_err());
        let next = table.open(9, ()).unwrap();
        assert_eq!(old.reject_before_execution(), None);
        assert!(next.queue());
        assert!(next.enter_resource());
        assert_eq!(next.reject_before_execution(), None);
        assert_eq!(next.finish_execution(false), Some(CallStatus::Failed));
    }
    #[test]
    fn advance_open_then_insert_open_publishes_the_same_record() {
        let mut table = CallTable::default();
        assert!(table.advance_open(5).is_ok());
        let call = table.insert_open(5, ()).unwrap();
        assert!(call.is_preparing_input());
        assert!(table.advance_open(5).is_err());
        assert!(table.advance_open(4).is_err());
        assert!(table.advance_open(0).is_err());
        assert!(matches!(table.continuation(5), CallLookup::Active(_)));
        // A second insert for the advanced ID is refused.
        assert!(table.insert_open(5, ()).is_err());
        // An insert without a fresh advance is refused.
        assert!(table.insert_open(6, ()).is_err());
        let next = table.open(6, ()).unwrap();
        assert!(next.queue());
    }
    #[test]
    fn refused_advance_keeps_continuations_retired_and_blocks_foreign_inserts() {
        let mut table = CallTable::default();
        assert!(table.advance_open(5).is_ok());
        // No active record exists, but continuations of the advanced ID are
        // Retired, never Future.
        assert!(matches!(table.continuation(5), CallLookup::Retired));
        assert!(matches!(table.continuation(6), CallLookup::Future));
        // Inserting a different ID without advancing it first is refused.
        assert!(matches!(
            table.insert_open(4, ()),
            Err("OPEN request ID is not increasing")
        ));
        assert!(matches!(
            table.insert_open(6, ()),
            Err("OPEN request ID is not increasing")
        ));
        // The advanced ID itself can still publish its record.
        assert!(table.insert_open(5, ()).is_ok());
        // A late insert for an older ID stays refused.
        assert!(table.insert_open(4, ()).is_err());
    }
    #[test]
    fn receive_positions_are_assigned_before_parallel_completion() {
        let mut value = ValueReceive::first(19, 8).unwrap();
        assert_eq!(value.geometry().unwrap(), (3, 8));
        assert_eq!(value.admit(8).unwrap(), 0);
        assert_eq!(value.admit(8).unwrap(), 1);
        assert_eq!(value.admit(3).unwrap(), 2);
        assert!(value.admit(3).is_err());
    }
}

#[cfg(test)]
mod concurrent_tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    #[test]
    fn competing_refusals_retire_once_and_keep_the_real_work_owner() {
        struct Slot(Arc<AtomicUsize>);
        impl Drop for Slot {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let slots = Arc::new(AtomicUsize::new(1));
        let mut table = CallTable::default();
        let work = table.open(1, Slot(slots.clone())).unwrap();
        let start = Arc::new(Barrier::new(9));
        let competitors: Vec<_> = (0..8)
            .map(|_| {
                let call = work.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    call.reject_before_execution().is_some()
                })
            })
            .collect();
        start.wait();
        assert_eq!(
            competitors
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
        drop(table.retire(1));
        assert!(matches!(table.continuation(1), CallLookup::Retired));
        assert!(!work.enter_resource());
        assert_eq!(slots.load(Ordering::Acquire), 1);
        drop(work);
        assert_eq!(slots.load(Ordering::Acquire), 0);
    }
}
