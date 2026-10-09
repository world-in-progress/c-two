use fastdb::{CompiledSpec, OpenOptions, Payload, PayloadError};

use c2_core::{AdapterFailurePhase, HeldResponse, PreparedCall};

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

/// Open a copy-backed FastDB payload while retaining the Core response lease.
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
/// invocation. Its `Drop` therefore invalidates the FastDB owner before Core
/// returns from the callback and releases the request lease.
pub struct BorrowedPayload {
    payload: Option<Payload>,
}

impl BorrowedPayload {
    pub fn payload(&self) -> &Payload {
        self.payload
            .as_ref()
            .expect("borrowed payload is live until its guard is dropped")
    }
}

impl Drop for BorrowedPayload {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            let _ = payload.invalidate();
            drop(payload);
        }
    }
}

/// Open one generated service input whose lifetime ends with its callback.
pub fn open_borrowed(spec: &CompiledSpec, bytes: &[u8]) -> Result<BorrowedPayload, PayloadError> {
    Ok(BorrowedPayload {
        payload: Some(Payload::open_copy(spec, bytes, &OpenOptions::default())?),
    })
}
