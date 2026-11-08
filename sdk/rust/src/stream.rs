use std::time::Duration;

use c2_core::{CoreResponseStream, HeldResponse};

use crate::{Error, StreamCloseReceipt};

/// A typed server response stream. Each item owns its result independently.
///
/// Dropping the stream submits cancellation without waiting for cleanup. Call
/// [`Self::close`] to observe the actual terminal state and item count.
#[must_use = "response streams must be consumed or explicitly closed"]
pub struct ResponseStream<T> {
    core: CoreResponseStream,
    decode: fn(HeldResponse) -> Result<T, Error>,
    ended: bool,
}

impl<T> ResponseStream<T> {
    #[doc(hidden)]
    pub fn from_core(
        core: CoreResponseStream,
        decode: fn(HeldResponse) -> Result<T, Error>,
    ) -> Self {
        Self {
            core,
            decode,
            ended: false,
        }
    }

    /// Submit cancellation, then observe up to the default 30 second budget.
    pub fn close(&self) -> StreamCloseReceipt {
        self.core.close()
    }

    /// Submit cancellation, then observe the original call's terminal state.
    /// `None` waits indefinitely; zero duration performs a single observation.
    pub fn close_with_timeout(&self, timeout: Option<Duration>) -> StreamCloseReceipt {
        self.core.close_with_timeout(timeout)
    }
}

impl<T> Iterator for ResponseStream<T> {
    type Item = Result<T, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        match self.core.next_item() {
            Ok(Some(response)) => {
                let decoded = (self.decode)(response);
                if let Err(error) = self.core.check_deadline() {
                    self.ended = true;
                    return Some(Err(error));
                }
                match decoded {
                    Ok(value) => Some(Ok(value)),
                    Err(error) => {
                        let _ = self.core.cancel();
                        self.ended = true;
                        Some(Err(error))
                    }
                }
            }
            Ok(None) => {
                self.ended = true;
                None
            }
            Err(error) => {
                self.ended = true;
                Some(Err(error))
            }
        }
    }
}
