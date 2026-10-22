//! Bounded, versioned owner-control payloads, independent of business frames.
//!
//! The enclosing frame supplies `FLAG_CTRL`, `FLAG_RESPONSE`, and the request ID.
//! Transports must check [`MAX_OWNER_PAYLOAD_SIZE`] before allocating the body;
//! these codecs also enforce that limit on standalone payloads. Identity and
//! endpoint authorization belong to Core, not this representation layer.

use std::fmt;

use crate::msg_type::MsgType;

pub const OWNER_CONTROL_VERSION: u8 = 1;
/// Maximum complete payload, including tag, version, and operation bytes.
pub const MAX_OWNER_PAYLOAD_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerWireError {
    Malformed,
    UnsupportedVersion,
    PayloadTooLarge,
}

impl fmt::Display for OwnerWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed owner control payload",
            Self::UnsupportedVersion => "unsupported owner control version",
            Self::PayloadTooLarge => "owner control payload exceeds size limit",
        })
    }
}

impl std::error::Error for OwnerWireError {}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerPhaseWire {
    Armed = 1,
    OwnerMissing = 2,
    Draining = 3,
    Finished = 4,
    WatcherIoError = 5,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerRejection {
    Malformed = 1,
    UnsupportedVersion = 2,
    IdentityMismatch = 3,
    OwnerPresent = 4,
    InvalidGrant = 5,
    GraceExpired = 6,
    ShuttingDown = 7,
    Unavailable = 8,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OwnerTicketWire {
    pub address: String,
    pub server_id: String,
    pub server_instance_id: String,
    pub epoch: u64,
    pub context_binding: [u8; 32],
    pub secret: [u8; 32],
}

impl fmt::Debug for OwnerTicketWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnerTicketWire")
            .field("address", &self.address)
            .field("server_id", &self.server_id)
            .field("server_instance_id", &self.server_instance_id)
            .field("epoch", &self.epoch)
            .field("context_binding", &self.context_binding)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerStatusWire {
    pub address: String,
    pub server_id: String,
    pub server_instance_id: String,
    pub epoch: u64,
    pub context_binding: [u8; 32],
    pub phase: OwnerPhaseWire,
}

#[derive(Clone, PartialEq, Eq)]
pub enum OwnerRequest {
    Hello,
    GetRecoveryTicket,
    RevokeRecovery,
    Takeover {
        ticket: OwnerTicketWire,
        successor_secret: [u8; 32],
    },
    RequestShutdown,
}

impl fmt::Debug for OwnerRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello => f.write_str("Hello"),
            Self::GetRecoveryTicket => f.write_str("GetRecoveryTicket"),
            Self::RevokeRecovery => f.write_str("RevokeRecovery"),
            Self::Takeover { ticket, .. } => f
                .debug_struct("Takeover")
                .field("ticket", ticket)
                .field("successor_secret", &"[REDACTED]")
                .finish(),
            Self::RequestShutdown => f.write_str("RequestShutdown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerResponse {
    Status(OwnerStatusWire),
    RecoveryTicket(OwnerTicketWire),
    Revoked,
    TakenOver(OwnerStatusWire),
    ShutdownInitiated,
    Rejected(OwnerRejection),
}

pub fn encode_owner_request(request: &OwnerRequest) -> Result<Vec<u8>, OwnerWireError> {
    let op = match request {
        OwnerRequest::Hello => 1,
        OwnerRequest::GetRecoveryTicket => 2,
        OwnerRequest::RevokeRecovery => 3,
        OwnerRequest::Takeover { .. } => 4,
        OwnerRequest::RequestShutdown => 5,
    };
    let mut payload = header(op);
    if let OwnerRequest::Takeover {
        ticket,
        successor_secret,
    } = request
    {
        write_ticket(&mut payload, ticket)?;
        write_bytes(&mut payload, successor_secret)?;
    }
    Ok(payload)
}

pub fn decode_owner_request(payload: &[u8]) -> Result<OwnerRequest, OwnerWireError> {
    let (op, mut body) = read_header(payload)?;
    let request = match op {
        1 => OwnerRequest::Hello,
        2 => OwnerRequest::GetRecoveryTicket,
        3 => OwnerRequest::RevokeRecovery,
        4 => OwnerRequest::Takeover {
            ticket: body.ticket()?,
            successor_secret: body.fixed()?,
        },
        5 => OwnerRequest::RequestShutdown,
        _ => return Err(OwnerWireError::Malformed),
    };
    body.finish()?;
    Ok(request)
}

pub fn encode_owner_response(response: &OwnerResponse) -> Result<Vec<u8>, OwnerWireError> {
    let op = match response {
        OwnerResponse::Status(_) => 1,
        OwnerResponse::RecoveryTicket(_) => 2,
        OwnerResponse::Revoked => 3,
        OwnerResponse::TakenOver(_) => 4,
        OwnerResponse::ShutdownInitiated => 5,
        OwnerResponse::Rejected(_) => 255,
    };
    let mut payload = header(op);
    match response {
        OwnerResponse::Status(status) | OwnerResponse::TakenOver(status) => {
            write_identity(
                &mut payload,
                &status.address,
                &status.server_id,
                &status.server_instance_id,
                status.epoch,
                &status.context_binding,
            )?;
            write_bytes(&mut payload, &[status.phase as u8])?;
        }
        OwnerResponse::RecoveryTicket(ticket) => write_ticket(&mut payload, ticket)?,
        OwnerResponse::Rejected(reason) => write_bytes(&mut payload, &[*reason as u8])?,
        OwnerResponse::Revoked | OwnerResponse::ShutdownInitiated => {}
    }
    Ok(payload)
}

pub fn decode_owner_response(payload: &[u8]) -> Result<OwnerResponse, OwnerWireError> {
    let (op, mut body) = read_header(payload)?;
    let response = match op {
        1 => OwnerResponse::Status(body.status()?),
        2 => OwnerResponse::RecoveryTicket(body.ticket()?),
        3 => OwnerResponse::Revoked,
        4 => OwnerResponse::TakenOver(body.status()?),
        5 => OwnerResponse::ShutdownInitiated,
        255 => OwnerResponse::Rejected(match body.byte()? {
            1 => OwnerRejection::Malformed,
            2 => OwnerRejection::UnsupportedVersion,
            3 => OwnerRejection::IdentityMismatch,
            4 => OwnerRejection::OwnerPresent,
            5 => OwnerRejection::InvalidGrant,
            6 => OwnerRejection::GraceExpired,
            7 => OwnerRejection::ShuttingDown,
            8 => OwnerRejection::Unavailable,
            _ => return Err(OwnerWireError::Malformed),
        }),
        _ => return Err(OwnerWireError::Malformed),
    };
    body.finish()?;
    Ok(response)
}

fn header(op: u8) -> Vec<u8> {
    vec![MsgType::OwnerControl.as_byte(), OWNER_CONTROL_VERSION, op]
}

fn read_header(payload: &[u8]) -> Result<(u8, Reader<'_>), OwnerWireError> {
    if payload.len() > MAX_OWNER_PAYLOAD_SIZE {
        return Err(OwnerWireError::PayloadTooLarge);
    }
    let mut reader = Reader(payload);
    if reader.byte()? != MsgType::OwnerControl.as_byte() {
        return Err(OwnerWireError::Malformed);
    }
    if reader.byte()? != OWNER_CONTROL_VERSION {
        return Err(OwnerWireError::UnsupportedVersion);
    }
    Ok((reader.byte()?, reader))
}

fn write_bytes(payload: &mut Vec<u8>, bytes: &[u8]) -> Result<(), OwnerWireError> {
    if bytes.len() > MAX_OWNER_PAYLOAD_SIZE - payload.len() {
        return Err(OwnerWireError::PayloadTooLarge);
    }
    payload.extend_from_slice(bytes);
    Ok(())
}

fn write_text(payload: &mut Vec<u8>, text: &str) -> Result<(), OwnerWireError> {
    let len = u16::try_from(text.len()).map_err(|_| OwnerWireError::PayloadTooLarge)?;
    write_bytes(payload, &len.to_le_bytes())?;
    write_bytes(payload, text.as_bytes())
}

fn write_identity(
    payload: &mut Vec<u8>,
    address: &str,
    server_id: &str,
    server_instance_id: &str,
    epoch: u64,
    context_binding: &[u8; 32],
) -> Result<(), OwnerWireError> {
    write_text(payload, address)?;
    write_text(payload, server_id)?;
    write_text(payload, server_instance_id)?;
    write_bytes(payload, &epoch.to_le_bytes())?;
    write_bytes(payload, context_binding)
}

fn write_ticket(payload: &mut Vec<u8>, ticket: &OwnerTicketWire) -> Result<(), OwnerWireError> {
    write_identity(
        payload,
        &ticket.address,
        &ticket.server_id,
        &ticket.server_instance_id,
        ticket.epoch,
        &ticket.context_binding,
    )?;
    write_bytes(payload, &ticket.secret)
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8], OwnerWireError> {
        let bytes = self.0.get(..len).ok_or(OwnerWireError::Malformed)?;
        self.0 = &self.0[len..];
        Ok(bytes)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], OwnerWireError> {
        self.take(N)?
            .try_into()
            .map_err(|_| OwnerWireError::Malformed)
    }

    fn byte(&mut self) -> Result<u8, OwnerWireError> {
        Ok(self.fixed::<1>()?[0])
    }

    fn text(&mut self) -> Result<String, OwnerWireError> {
        let len = u16::from_le_bytes(self.fixed()?) as usize;
        let text = std::str::from_utf8(self.take(len)?).map_err(|_| OwnerWireError::Malformed)?;
        Ok(text.to_owned())
    }

    fn ticket(&mut self) -> Result<OwnerTicketWire, OwnerWireError> {
        Ok(OwnerTicketWire {
            address: self.text()?,
            server_id: self.text()?,
            server_instance_id: self.text()?,
            epoch: u64::from_le_bytes(self.fixed()?),
            context_binding: self.fixed()?,
            secret: self.fixed()?,
        })
    }

    fn status(&mut self) -> Result<OwnerStatusWire, OwnerWireError> {
        Ok(OwnerStatusWire {
            address: self.text()?,
            server_id: self.text()?,
            server_instance_id: self.text()?,
            epoch: u64::from_le_bytes(self.fixed()?),
            context_binding: self.fixed()?,
            phase: match self.byte()? {
                1 => OwnerPhaseWire::Armed,
                2 => OwnerPhaseWire::OwnerMissing,
                3 => OwnerPhaseWire::Draining,
                4 => OwnerPhaseWire::Finished,
                5 => OwnerPhaseWire::WatcherIoError,
                _ => return Err(OwnerWireError::Malformed),
            },
        })
    }

    fn finish(self) -> Result<(), OwnerWireError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(OwnerWireError::Malformed)
        }
    }
}
