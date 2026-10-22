//! Owner-channel framing. Core alone decides who owns a host.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use c2_local::{AbortHandle, EndpointContextBinding, LocalReadHalf, LocalStream, LocalWriteHalf};
use c2_wire::flags::{FLAG_CTRL, FLAG_RESPONSE};
use c2_wire::frame::{decode_frame_body, encode_frame};
use c2_wire::owner_control::{
    MAX_OWNER_PAYLOAD_SIZE, OWNER_CONTROL_VERSION, OwnerRejection, OwnerRequest, OwnerResponse,
    OwnerWireError, decode_owner_request, encode_owner_response,
};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

use crate::Connection;

/// The fixed Core hook installed before a server starts. Selecting this
/// protocol role grants no permission to change ownership or admission.
#[doc(hidden)]
pub trait OwnerControlHandler: Send + Sync {
    fn bound(&self, binding: EndpointContextBinding) -> io::Result<()>;
    fn setup_deadline(&self, started: Instant) -> Instant;
    fn serve(
        self: Arc<Self>,
        session: OwnerSession,
        first: OwnerMessage,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

#[doc(hidden)]
pub struct OwnerMessage {
    pub request_id: u64,
    pub request: OwnerRequest,
}

#[derive(Debug)]
pub enum OwnerReadError {
    Io(io::Error),
    Protocol {
        request_id: u64,
        error: OwnerWireError,
    },
}

/// One reader and one writer, used by both inherited and accepted owner
/// channels. Cancelling a read ends this session; partial frames are never
/// handed to another operation or interpreted as a new message.
#[doc(hidden)]
pub struct OwnerSession {
    reader: LocalReadHalf,
    writer: Arc<Mutex<LocalWriteHalf>>,
    abort: AbortHandle,
    connection: Option<Arc<Connection>>,
}

impl OwnerSession {
    pub fn new(stream: LocalStream) -> Self {
        let abort = stream.abort_handle();
        let (reader, writer) = stream.into_split();
        Self {
            reader,
            writer: Arc::new(Mutex::new(writer)),
            abort,
            connection: None,
        }
    }

    pub(crate) fn accepted(
        reader: LocalReadHalf,
        writer: Arc<Mutex<LocalWriteHalf>>,
        abort: AbortHandle,
        connection: Arc<Connection>,
    ) -> Self {
        Self {
            reader,
            writer,
            abort,
            connection: Some(connection),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.abort.is_aborted()
            || self
                .connection
                .as_ref()
                .is_some_and(|c| c.connection_observation().snapshot().is_some())
    }

    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }

    pub(crate) fn decode(
        request_id: u64,
        flags: u32,
        payload: &[u8],
    ) -> Result<OwnerMessage, OwnerReadError> {
        if flags != FLAG_CTRL {
            return Err(OwnerReadError::Protocol {
                request_id,
                error: OwnerWireError::Malformed,
            });
        }
        decode_owner_request(payload)
            .map(|request| OwnerMessage {
                request_id,
                request,
            })
            .map_err(|error| OwnerReadError::Protocol { request_id, error })
    }

    pub async fn read(&mut self) -> Result<OwnerMessage, OwnerReadError> {
        let result = self.read_frame().await;
        if let Err(OwnerReadError::Protocol { request_id, error }) = &result {
            let rejection = if *error == OwnerWireError::UnsupportedVersion {
                OwnerRejection::UnsupportedVersion
            } else {
                OwnerRejection::Malformed
            };
            let _ = self
                .reply(*request_id, &OwnerResponse::Rejected(rejection))
                .await;
        }
        if let (Err(error), Some(conn)) = (&result, &self.connection) {
            match error {
                OwnerReadError::Io(error) => conn.end_read("owner_read", error),
                OwnerReadError::Protocol { .. } => conn.end(
                    c2_local::ConnectionEndReason::ProtocolError,
                    "owner_protocol",
                ),
            }
        }
        result
    }

    async fn read_frame(&mut self) -> Result<OwnerMessage, OwnerReadError> {
        let mut header = [0; 16];
        self.reader
            .read_exact(&mut header)
            .await
            .map_err(OwnerReadError::Io)?;
        let length = u32::from_le_bytes(header[..4].try_into().unwrap());
        let request_id = u64::from_le_bytes(header[4..12].try_into().unwrap());
        if !(15..=12 + MAX_OWNER_PAYLOAD_SIZE as u32).contains(&length) {
            return Err(OwnerReadError::Protocol {
                request_id,
                error: OwnerWireError::Malformed,
            });
        }
        let (frame, _) = decode_frame_body(&header[4..], length).expect("fixed owner frame header");
        if frame.flags != FLAG_CTRL {
            return Err(OwnerReadError::Protocol {
                request_id,
                error: OwnerWireError::Malformed,
            });
        }
        let mut prefix = [0; 3];
        self.reader
            .read_exact(&mut prefix)
            .await
            .map_err(OwnerReadError::Io)?;
        let error = if prefix[0] != c2_wire::msg_type::MsgType::OwnerControl.as_byte() {
            Some(OwnerWireError::Malformed)
        } else if prefix[1] != OWNER_CONTROL_VERSION {
            Some(OwnerWireError::UnsupportedVersion)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(OwnerReadError::Protocol { request_id, error });
        }
        let mut payload = vec![0; frame.payload_len()];
        payload[..3].copy_from_slice(&prefix);
        self.reader
            .read_exact(&mut payload[3..])
            .await
            .map_err(OwnerReadError::Io)?;
        Self::decode(frame.request_id, frame.flags, &payload)
    }

    pub async fn reply(&self, request_id: u64, response: &OwnerResponse) -> io::Result<()> {
        let payload = encode_owner_response(response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let frame = encode_frame(request_id, FLAG_CTRL | FLAG_RESPONSE, &payload);
        let result = self.writer.lock().await.write_all(&frame).await;
        if let (Err(error), Some(conn)) = (&result, &self.connection) {
            conn.end_io("owner_write", error);
        }
        result
    }

    /// Reserve the existing drain guard before Core can initiate shutdown.
    /// The terminal reply uses the ordinary control-plane peer-close budget.
    pub async fn respond(
        &mut self,
        message: OwnerMessage,
        authorize: impl FnOnce(OwnerRequest) -> OwnerResponse,
    ) -> io::Result<bool> {
        let shutdown_request = matches!(message.request, OwnerRequest::RequestShutdown);
        let connection = if shutdown_request {
            self.connection.clone()
        } else {
            None
        };
        let _flight = connection
            .as_ref()
            .map(|connection| crate::connection::FlightGuard::new(connection));
        let response = authorize(message.request);
        let initiated = matches!(response, OwnerResponse::ShutdownInitiated);
        if shutdown_request {
            tokio::time::timeout(
                std::time::Duration::from_millis(crate::server::CONTROL_ACK_PEER_CLOSE_TIMEOUT_MS),
                async {
                    self.reply(message.request_id, &response).await?;
                    if initiated {
                        let mut byte = [0];
                        // EOF proves the client has ended its control session.
                        // A timeout ends only this courtesy reply, never drain.
                        let _ = self.reader.read(&mut byte).await;
                    }
                    Ok::<_, io::Error>(())
                },
            )
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "owner terminal reply deadline")
            })??;
        } else {
            self.reply(message.request_id, &response).await?;
        }
        Ok(initiated)
    }
}

impl Drop for OwnerSession {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
