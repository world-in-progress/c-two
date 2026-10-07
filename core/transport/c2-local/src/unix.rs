use super::{
    EndpointCredential, EndpointInspection, EndpointReapResult, LocalEndpoint, SweepBatch,
    SweepBudget,
};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::net::UnixStream;

pub type Stream = UnixStream;

pub async fn connect(endpoint: &LocalEndpoint) -> io::Result<Stream> {
    UnixStream::connect(Path::new(endpoint.os_name())).await
}

pub fn raw_handle(stream: &Stream) -> usize {
    stream.as_raw_fd() as usize
}

pub fn cancel(raw: usize) {
    unsafe {
        libc::shutdown(raw as i32, libc::SHUT_RDWR);
    }
}

pub fn poll_flush(stream: &mut Stream, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    Pin::new(stream).poll_flush(cx)
}

pub type Listener = crate::unix_managed::ManagedListener;

pub(crate) fn inspect_endpoint(endpoint: &LocalEndpoint) -> EndpointInspection {
    crate::unix_managed::inspect_managed(endpoint)
}

pub(crate) fn reap_endpoint(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    crate::unix_managed::reap_managed(endpoint, credential)
}

pub(crate) struct EndpointSweep(crate::unix_managed::ManagedSweep);
impl EndpointSweep {
    pub(crate) fn open() -> io::Result<Self> {
        Self::for_endpoint(&LocalEndpoint::from_address("ipc://c2-endpoint-sweep")?)
    }
    pub(crate) fn for_endpoint(endpoint: &LocalEndpoint) -> io::Result<Self> {
        crate::unix_managed::ManagedSweep::for_endpoint(endpoint).map(Self)
    }
    pub(crate) fn for_scope(
        endpoint: &LocalEndpoint,
        targets: &[LocalEndpoint],
    ) -> io::Result<Self> {
        crate::unix_managed::ManagedSweep::for_scope(endpoint, targets).map(Self)
    }
    pub(crate) fn next_batch(&mut self, budget: SweepBudget) -> SweepBatch {
        self.0.next_batch(budget)
    }
}
