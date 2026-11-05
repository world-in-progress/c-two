use fastdb::{CompiledSpec, OpenOptions, Payload, PayloadError};

use c2_core::{
    AdapterFailure, AdapterFailurePhase, EncodedStream, EncodedStreamItem, HeldResponse,
    PreparedCall, normalize_adapter_failure,
};
use c2_error::C2Error;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{Error, Held};

/// Charge a portable input before the existing serializer makes its owned copy.
///
/// FastDB's checked access supplies the byte length without copying or parsing
/// its layout. The borrowed serializer stays on the caller thread; its work is
/// covered by the original Core deadline, but is not hard real time. The
/// serializer must still validate the expected specification and live owner.
pub fn charge_payload_input(call: &mut PreparedCall, payload: &Payload) -> Result<(), Error> {
    let phase = AdapterFailurePhase::ClientInputSerializing;
    let access = payload
        .acquire()
        .map_err(|error| Error::Semantic(crate::error::fastdb_adapter_error(phase, error)))?;
    let bytes = access
        .payload_bytes()
        .map_err(|error| Error::Semantic(crate::error::fastdb_adapter_error(phase, error)))?;
    let nbytes = u64::try_from(bytes.len())
        .map_err(|_| c2_core::LifecycleError::Configuration("input length exceeds u64".into()))?;
    call.charge_input(nbytes)
}

/// Open an owned FastDB copy from detached response bytes.
///
/// Generated code remains responsible for selecting the exact adapter phase
/// and checking the contract-owned expected specification digest.
pub fn open_owned(spec: &CompiledSpec, bytes: &[u8]) -> Result<Payload, PayloadError> {
    Payload::open_copy(spec, bytes, &OpenOptions::default())
}

/// Open a copy-backed FastDB payload while retaining the local response copy.
pub fn open_held(
    spec: &CompiledSpec,
    response: HeldResponse,
) -> Result<Held<Payload>, PayloadError> {
    let payload = Payload::open_copy(spec, response.bytes(), &OpenOptions::default())?;
    Ok(Held::new(payload, response, invalidate_payload))
}

fn invalidate_payload(payload: &Payload) -> Result<(), String> {
    payload.invalidate().map_err(|error| error.to_string())
}

/// Callback-scoped owner for a generated service's copy-backed FastDB input.
///
/// Generated adapters construct this guard inside [`c2_core::EncodedService`]
/// invocation, after Core has released the request transport storage. Its `Drop`
/// invalidates the FastDB owner before the callback-scoped local copy ends.
pub struct BorrowedPayload {
    payload: Payload,
    invalidated: bool,
}

impl BorrowedPayload {
    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    /// End the checked input lifetime, including when invalidation fails.
    pub fn invalidate(&mut self) -> Result<(), PayloadError> {
        if self.invalidated {
            return Ok(());
        }
        self.invalidated = true;
        self.payload.invalidate()
    }
}

impl Drop for BorrowedPayload {
    fn drop(&mut self) {
        if let Err(error) = self.invalidate() {
            eprintln!("Warning: borrowed payload invalidation failed: {error}");
        }
    }
}

/// Open one generated service input whose lifetime ends with its callback.
pub fn open_borrowed(spec: &CompiledSpec, bytes: &[u8]) -> Result<BorrowedPayload, PayloadError> {
    Ok(BorrowedPayload {
        payload: Payload::open_copy(spec, bytes, &OpenOptions::default())?,
        invalidated: false,
    })
}

/// An owned Rust resource iterator and its callback input, retained until the
/// Core cleanup worker calls `finish` after the last resource step.
#[doc(hidden)]
pub struct PayloadStream {
    iterator: Option<Box<dyn Iterator<Item = Result<Payload, Error>> + Send>>,
    input: Option<BorrowedPayload>,
    output_sha256: [u8; 32],
    method: &'static str,
    method_index: u16,
}

impl PayloadStream {
    pub fn new(
        iterator: Box<dyn Iterator<Item = Result<Payload, Error>> + Send>,
        input: Option<BorrowedPayload>,
        output_sha256: [u8; 32],
        method: &'static str,
        method_index: u16,
    ) -> Self {
        Self {
            iterator: Some(iterator),
            input,
            output_sha256,
            method,
            method_index,
        }
    }

    fn panic_error(&self, operation: &str) -> C2Error {
        normalize_adapter_failure(
            AdapterFailurePhase::ResourceFunctionExecuting,
            AdapterFailure::Local {
                message: format!("service method {:?} {operation} panicked", self.method),
                details: std::collections::BTreeMap::from([
                    ("method".to_string(), self.method.to_string()),
                    ("method_index".to_string(), self.method_index.to_string()),
                    ("operation".to_string(), operation.to_string()),
                ]),
            },
        )
    }
}

impl EncodedStream for PayloadStream {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        let iterator = self
            .iterator
            .as_mut()
            .expect("stream iterator finished only once");
        let item = catch_unwind(AssertUnwindSafe(|| iterator.next()))
            .map_err(|_| self.panic_error("next"))?;
        match item {
            Some(Ok(payload)) => Ok(Some(Box::new(OwnedPayloadStreamItem::new(
                payload,
                &self.output_sha256,
            )?))),
            Some(Err(error)) => Err(crate::error::adapter_error(
                AdapterFailurePhase::ResourceFunctionExecuting,
                error,
            )),
            None => Ok(None),
        }
    }

    fn finish(mut self: Box<Self>) -> Result<(), C2Error> {
        let iterator_result = catch_unwind(AssertUnwindSafe(|| drop(self.iterator.take())));
        let input_result = catch_unwind(AssertUnwindSafe(|| {
            self.input.as_mut().map(BorrowedPayload::invalidate)
        }));
        let result = match iterator_result {
            Ok(()) => Ok(()),
            Err(_) => Err(self.panic_error("drop")),
        };
        let input_result = match input_result {
            Ok(Some(Err(error))) => Err(crate::error::fastdb_adapter_error(
                AdapterFailurePhase::ResourceFunctionExecuting,
                error,
            )),
            Err(_) => Err(self.panic_error("input cleanup")),
            _ => Ok(()),
        };
        match (result, input_result) {
            (Err(primary), Err(secondary)) => {
                eprintln!("Warning: additional Rust stream cleanup error: {secondary}");
                Err(primary)
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        }
    }
}

struct OwnedPayloadStreamItem {
    payload: Payload,
    len: usize,
}

impl OwnedPayloadStreamItem {
    fn new(payload: Payload, expected_sha256: &[u8; 32]) -> Result<Self, C2Error> {
        let phase = AdapterFailurePhase::ResourceOutputSerializing;
        payload
            .require_spec_sha256(expected_sha256)
            .map_err(|error| crate::error::fastdb_adapter_error(phase, error))?;
        let len = payload
            .acquire()
            .and_then(|access| access.payload_bytes().map(<[u8]>::len))
            .map_err(|error| crate::error::fastdb_adapter_error(phase, error))?;
        Ok(Self { payload, len })
    }
}

impl EncodedStreamItem for OwnedPayloadStreamItem {
    fn len(&self) -> usize {
        self.len
    }

    fn write_range(&self, offset: usize, dst: &mut [u8]) -> Result<(), C2Error> {
        let access = self.payload.acquire().map_err(|error| {
            crate::error::fastdb_adapter_error(
                AdapterFailurePhase::ResourceOutputSerializing,
                error,
            )
        })?;
        let bytes = access.payload_bytes().map_err(|error| {
            crate::error::fastdb_adapter_error(
                AdapterFailurePhase::ResourceOutputSerializing,
                error,
            )
        })?;
        dst.copy_from_slice(&bytes[offset..offset + dst.len()]);
        Ok(())
    }
}
