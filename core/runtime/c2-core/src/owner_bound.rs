//! One owner epoch, one recovery grant, and the existing host drain transaction.

use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use c2_config::ServerLifecyclePolicy;
use c2_local::{EndpointContextBinding, OwnerBootstrap};
use c2_server::owner_control::{OwnerControlHandler, OwnerMessage, OwnerReadError, OwnerSession};
use c2_server::{OwnerAdmissionControl, Server, ServerIdentity};
use c2_wire::owner_control::{
    OwnerPhaseWire, OwnerRejection, OwnerRequest, OwnerResponse, OwnerStatusWire, OwnerTicketWire,
};
use parking_lot::Mutex;
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

/// Observable lifecycle phase of one host.
///
/// Phases never expose OS handles or the capability itself. `WatcherIoError`
/// is sticky: it records why owner observation ended and is not overwritten
/// by the later `Finished` transition of the shutdown transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLifecyclePhase {
    /// The host runs with the `Persistent` policy; no owner watcher exists
    /// and only explicit shutdown ends it.
    Persistent,
    /// The owner control watcher is armed and the controller end is still
    /// open. This is the phase from before readiness is published.
    Armed,
    /// The controller relationship ended (control EOF or watcher failure).
    /// New business admission is already suspended; the bounded grace window
    /// is running and in-flight work continues under the original rules.
    OwnerMissing,
    /// The grace window ended and the native drain/shutdown transaction is
    /// running.
    Draining,
    /// The host's shutdown transaction completed and produced one outcome.
    Finished,
    /// The owner watcher itself failed, so the owner relationship can no
    /// longer be observed. Admission is closed and the same bounded shutdown
    /// sequence runs; this is not a business-stream failure and not an owner
    /// EOF.
    WatcherIoError(String),
}

/// Server-direction transport charges still retained by payload owners.
///
/// These are held payload references (response and reassembly backing), not
/// process memory. A stopped server does not zero them, a completed shutdown
/// transaction does not release them, and endpoint cleanup never releases or
/// falsifies them: they drop only when the actual payload owners release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostClientHeldLeases {
    /// Retained shared-memory charges in the server response direction.
    pub response_shm_bytes: u64,
    /// Retained file-spill charges in the server response direction.
    pub response_file_bytes: u64,
    /// Retained chunk-reassembly charges.
    pub reassembly_bytes: u64,
}

/// Read-only host lifecycle observation.
///
/// `listener_closed`, `work_drained`, and `client_held_leases` are separate
/// observations: a closed listener does not prove drained work, and drained
/// server work does not prove client-held payload references were released.
/// This snapshot does not report endpoint (socket/lock) cleanup results;
/// that projection belongs to the later endpoint-reclamation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLifecycleSnapshot {
    pub policy: ServerLifecyclePolicy,
    pub phase: HostLifecyclePhase,
    /// No listener is open: either the server never bound, or the shutdown
    /// transaction closed its listener.
    pub listener_closed: bool,
    /// Whether every recorded route close drained its active work. `None`
    /// until a shutdown transaction produced route outcomes.
    pub work_drained: Option<bool>,
    pub client_held_leases: HostClientHeldLeases,
}

struct OwnerState {
    phase: HostLifecyclePhase,
    epoch: u64,
    grant: Option<[u8; 32]>,
    deadline: Option<Instant>,
    binding: Option<EndpointContextBinding>,
}

/// The lifecycle snapshot and protocol supervisor read the same state.
#[derive(Clone)]
pub(crate) struct HostLifecycleCell {
    state: Arc<Mutex<OwnerState>>,
    changed: Arc<Notify>,
}

impl HostLifecycleCell {
    pub(crate) fn new(policy: &ServerLifecyclePolicy) -> Self {
        Self {
            state: Arc::new(Mutex::new(OwnerState {
                phase: if policy.is_owner_bound() {
                    HostLifecyclePhase::Armed
                } else {
                    HostLifecyclePhase::Persistent
                },
                epoch: 1,
                grant: None,
                deadline: None,
                binding: None,
            })),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn phase(&self) -> HostLifecyclePhase {
        self.state.lock().phase.clone()
    }

    pub(crate) fn finish_from_outcome(&self, outcome: &crate::ShutdownOutcome) {
        let mut state = self.state.lock();
        state.grant = None;
        state.deadline = None;
        if !matches!(state.phase, HostLifecyclePhase::WatcherIoError(_))
            && outcome.runtime_barrier_error.is_none()
            && outcome.route_close_error.is_none()
        {
            state.phase = HostLifecyclePhase::Finished;
        }
        self.changed.notify_waiters();
    }
}

pub(crate) enum OwnerArmOutcome {
    Alive(OwnerSession),
    Closed,
    IoError(String),
}

pub(crate) async fn arm_owner_watch(bootstrap: &mut OwnerBootstrap) -> OwnerArmOutcome {
    match bootstrap.activate().await {
        Ok(stream) => OwnerArmOutcome::Alive(OwnerSession::new(stream)),
        Err(error) if peer_ended(&error) => OwnerArmOutcome::Closed,
        Err(error) => OwnerArmOutcome::IoError(error.to_string()),
    }
}

pub(crate) async fn arm_owner_watch_with_fence<F>(
    prepare: F,
    server: &Server,
    timeout: Duration,
) -> OwnerArmOutcome
where
    F: std::future::Future<Output = OwnerArmOutcome>,
{
    tokio::select! {
        biased;
        _ = server.wait_for_shutdown_requested() => OwnerArmOutcome::IoError("owner control establishment cancelled by shutdown".into()),
        _ = tokio::time::sleep(timeout) => OwnerArmOutcome::IoError("owner control establishment exceeded startup deadline".into()),
        outcome = prepare => outcome,
    }
}

/// The construction-only admission authority never leaves this Core owner.
/// All owner mutations lock state before admission; neither lock crosses await.
pub(crate) struct OwnerSupervisor {
    lifecycle: HostLifecycleCell,
    admission: OwnerAdmissionControl,
    server: OnceLock<Weak<Server>>,
    identity: ServerIdentity,
    address: String,
    grace: Duration,
}

impl OwnerSupervisor {
    pub(crate) fn new(
        lifecycle: HostLifecycleCell,
        admission: OwnerAdmissionControl,
        identity: ServerIdentity,
        address: String,
        grace: Duration,
    ) -> Self {
        Self {
            lifecycle,
            admission,
            server: OnceLock::new(),
            identity,
            address,
            grace,
        }
    }

    pub(crate) fn attach_server(&self, server: &Arc<Server>) {
        assert!(self.server.set(Arc::downgrade(server)).is_ok());
    }

    fn server(&self) -> Arc<Server> {
        self.server
            .get()
            .expect("owner supervisor attached before start")
            .upgrade()
            .expect("owner session runs inside its server lifetime")
    }

    fn status(&self, state: &OwnerState) -> Result<OwnerStatusWire, OwnerRejection> {
        let binding = state.binding.as_ref().ok_or(OwnerRejection::Unavailable)?;
        binding.validate().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                OwnerRejection::Unavailable
            } else {
                OwnerRejection::IdentityMismatch
            }
        })?;
        Ok(OwnerStatusWire {
            address: self.address.clone(),
            server_id: self.identity.server_id.clone(),
            server_instance_id: self.identity.server_instance_id.clone(),
            epoch: state.epoch,
            context_binding: binding.digest(),
            phase: match state.phase {
                HostLifecyclePhase::Armed => OwnerPhaseWire::Armed,
                HostLifecyclePhase::OwnerMissing => OwnerPhaseWire::OwnerMissing,
                HostLifecyclePhase::Draining => OwnerPhaseWire::Draining,
                HostLifecyclePhase::WatcherIoError(_) => OwnerPhaseWire::WatcherIoError,
                HostLifecyclePhase::Finished | HostLifecyclePhase::Persistent => {
                    OwnerPhaseWire::Finished
                }
            },
        })
    }

    /// The clock is checked at the commit point, independently of timer scheduling.
    fn expire(&self, state: &mut OwnerState, now: Instant) -> bool {
        if state.deadline.is_some_and(|deadline| now >= deadline) {
            let reason = if matches!(state.phase, HostLifecyclePhase::WatcherIoError(_)) {
                c2_server::OWNER_WATCHER_ERROR_SHUTDOWN_REASON
            } else {
                state.phase = HostLifecyclePhase::Draining;
                c2_server::OWNER_BOUND_SHUTDOWN_REASON
            };
            state.deadline = None;
            state.grant = None;
            self.server().request_shutdown_signal_with_reason(reason);
            self.lifecycle.changed.notify_waiters();
            true
        } else {
            false
        }
    }

    fn ended(&self, epoch: u64, error: Option<String>) {
        let mut state = self.lifecycle.state.lock();
        if state.epoch != epoch || !matches!(state.phase, HostLifecyclePhase::Armed) {
            return;
        }
        if !self.admission.pause() {
            state.phase = HostLifecyclePhase::Draining;
            state.grant = None;
            state.deadline = None;
            self.lifecycle.changed.notify_waiters();
            return;
        }
        state.phase = match error {
            Some(message) => {
                state.grant = None;
                HostLifecyclePhase::WatcherIoError(message)
            }
            None => HostLifecyclePhase::OwnerMissing,
        };
        state.deadline = Some(Instant::now() + self.grace);
        self.lifecycle.changed.notify_waiters();
    }

    fn shutdown_observed(&self) {
        let mut state = self.lifecycle.state.lock();
        state.grant = None;
        state.deadline = None;
        if !matches!(
            state.phase,
            HostLifecyclePhase::WatcherIoError(_) | HostLifecyclePhase::Finished
        ) {
            state.phase = HostLifecyclePhase::Draining;
        }
        self.lifecycle.changed.notify_waiters();
    }

    fn command(&self, epoch: u64, request: OwnerRequest) -> OwnerResponse {
        let mut state = self.lifecycle.state.lock();
        let result = (|| {
            if epoch != state.epoch || !matches!(state.phase, HostLifecyclePhase::Armed) {
                return Err(OwnerRejection::InvalidGrant);
            }
            if !self.server().is_running() {
                return Err(OwnerRejection::ShuttingDown);
            }
            // A replaced namespace cannot issue authority for the old listener.
            let status = self.status(&state)?;
            Ok(match request {
                OwnerRequest::Hello => OwnerResponse::Status(status),
                OwnerRequest::GetRecoveryTicket => {
                    if state.grant.is_none() {
                        let mut secret = [0; 32];
                        getrandom::fill(&mut secret).map_err(|_| OwnerRejection::Unavailable)?;
                        state.grant = Some(secret);
                    }
                    OwnerResponse::RecoveryTicket(OwnerTicketWire {
                        address: status.address,
                        server_id: status.server_id,
                        server_instance_id: status.server_instance_id,
                        epoch: status.epoch,
                        context_binding: status.context_binding,
                        secret: state.grant.unwrap(),
                    })
                }
                OwnerRequest::RevokeRecovery => {
                    state.grant = None;
                    OwnerResponse::Revoked
                }
                OwnerRequest::RequestShutdown => {
                    state.grant = None;
                    state.phase = HostLifecyclePhase::Draining;
                    self.server()
                        .request_shutdown_signal_with_reason(c2_server::DIRECT_IPC_SHUTDOWN_REASON);
                    self.lifecycle.changed.notify_waiters();
                    OwnerResponse::ShutdownInitiated
                }
                OwnerRequest::Takeover { .. } => return Err(OwnerRejection::OwnerPresent),
            })
        })();
        result.unwrap_or_else(OwnerResponse::Rejected)
    }

    fn takeover(
        &self,
        session: &OwnerSession,
        ticket: OwnerTicketWire,
        successor_secret: [u8; 32],
        setup_deadline: Instant,
    ) -> Result<(u64, OwnerStatusWire), OwnerRejection> {
        let mut state = self.lifecycle.state.lock();
        if self.expire(&mut state, Instant::now()) {
            return Err(OwnerRejection::GraceExpired);
        }
        match state.phase {
            HostLifecyclePhase::OwnerMissing => {}
            HostLifecyclePhase::Armed => return Err(OwnerRejection::OwnerPresent),
            _ => return Err(OwnerRejection::ShuttingDown),
        }
        let status = self.status(&state)?;
        if ticket.address != status.address
            || ticket.server_id != status.server_id
            || ticket.server_instance_id != status.server_instance_id
            || ticket.context_binding != status.context_binding
        {
            return Err(OwnerRejection::IdentityMismatch);
        }
        if ticket.epoch != state.epoch
            || !state
                .grant
                .is_some_and(|secret| bool::from(secret.ct_eq(&ticket.secret)))
        {
            return Err(OwnerRejection::InvalidGrant);
        }
        if session.is_closed() {
            return Err(OwnerRejection::Unavailable);
        }
        let next_epoch = state
            .epoch
            .checked_add(1)
            .ok_or(OwnerRejection::Unavailable)?;
        // resume and terminal shutdown share the existing admission lock. A
        // shutdown which wins that lock cannot be reopened by this transaction.
        let deadline = state
            .deadline
            .expect("OwnerMissing has its original grace deadline")
            .min(setup_deadline);
        if !self.admission.resume(deadline) {
            if self.expire(&mut state, Instant::now()) {
                return Err(OwnerRejection::GraceExpired);
            }
            if Instant::now() >= setup_deadline {
                return Err(OwnerRejection::Unavailable);
            }
            return Err(OwnerRejection::ShuttingDown);
        }
        state.epoch = next_epoch;
        state.grant = Some(successor_secret);
        state.deadline = None;
        state.phase = HostLifecyclePhase::Armed;
        self.lifecycle.changed.notify_waiters();
        let mut status = status;
        status.epoch = next_epoch;
        status.phase = OwnerPhaseWire::Armed;
        Ok((next_epoch, status))
    }

    async fn active(self: &Arc<Self>, session: &mut OwnerSession, epoch: u64) {
        let server = self.server();
        loop {
            let message = tokio::select! {
                biased;
                _ = stopping(&server) => { self.shutdown_observed(); return; }
                result = session.read() => result,
            };
            match message {
                Ok(message) => {
                    let terminal_request = matches!(message.request, OwnerRequest::RequestShutdown);
                    let response = session.respond(message, |request| self.command(epoch, request));
                    let result = if terminal_request {
                        response.await
                    } else {
                        tokio::select! {
                            _ = stopping(&server) => { self.shutdown_observed(); return; }
                            result = response => result,
                        }
                    };
                    match result {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(error) => {
                            self.ended(epoch, (!peer_ended(&error)).then(|| error.to_string()));
                            return;
                        }
                    }
                }
                Err(OwnerReadError::Io(error)) => {
                    self.ended(epoch, (!peer_ended(&error)).then(|| error.to_string()));
                    return;
                }
                Err(OwnerReadError::Protocol { error, .. }) => {
                    self.ended(epoch, Some(error.to_string()));
                    return;
                }
            }
        }
    }

    async fn candidate(
        self: Arc<Self>,
        mut session: OwnerSession,
        first: OwnerMessage,
        deadline: Instant,
    ) {
        if Instant::now() >= deadline {
            return;
        }
        if !matches!(first.request, OwnerRequest::Hello) {
            let _ = session
                .reply(
                    first.request_id,
                    &OwnerResponse::Rejected(OwnerRejection::Malformed),
                )
                .await;
            return;
        }
        let server = self.server();
        let setup = async {
            let status = {
                let mut state = self.lifecycle.state.lock();
                self.expire(&mut state, Instant::now());
                self.status(&state)
            };
            let continuing =
                matches!(&status, Ok(status) if status.phase == OwnerPhaseWire::OwnerMissing);
            let response = status
                .map(OwnerResponse::Status)
                .unwrap_or_else(OwnerResponse::Rejected);
            session.reply(first.request_id, &response).await.ok()?;
            if !continuing {
                return None;
            }
            let message = session.read().await.ok()?;
            let result = match message.request {
                OwnerRequest::Takeover {
                    ticket,
                    successor_secret,
                } => self.takeover(&session, ticket, successor_secret, deadline),
                _ => Err(OwnerRejection::Malformed),
            };
            match result {
                Ok((epoch, status)) => {
                    // Ownership and supervision exist before writing the ACK.
                    // A lost ACK enters normal grace for this new epoch.
                    let relationship = OwnerRelationship {
                        owner: &self,
                        epoch,
                    };
                    session
                        .reply(message.request_id, &OwnerResponse::TakenOver(status))
                        .await
                        .ok()?;
                    Some(relationship)
                }
                Err(reason) => {
                    let _ = session
                        .reply(message.request_id, &OwnerResponse::Rejected(reason))
                        .await;
                    None
                }
            }
        };
        let adopted = tokio::select! {
            biased;
            _ = server.wait_for_shutdown_requested() => None,
            result = tokio::time::timeout_at(deadline.into(), setup) => result.ok().flatten(),
        };
        if let Some(relationship) = adopted {
            self.active(&mut session, relationship.epoch).await;
        }
    }

    /// One deadline observer for all epochs; stale wakeups recheck the current
    /// state, and no attempt can move an already running grace deadline.
    async fn watch_deadline(&self) {
        let server = self.server();
        loop {
            let changed = self.lifecycle.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = {
                let mut state = self.lifecycle.state.lock();
                if self.expire(&mut state, Instant::now()) {
                    return;
                }
                state.deadline
            };
            tokio::select! {
                _ = stopping(&server) => { self.shutdown_observed(); return; }
                _ = &mut changed => {}
                _ = async { if let Some(deadline) = deadline { tokio::time::sleep_until(deadline.into()).await; } else { std::future::pending::<()>().await; } } => {}
            }
        }
    }

    pub(crate) async fn supervise(self: Arc<Self>, mut bootstrap: OwnerSession) {
        let server = self.server();
        // run() binds the endpoint before publishing readiness. A private
        // controller may write early, but no unbound ticket can be issued.
        let ready = async {
            loop {
                let changed = self.lifecycle.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.lifecycle.state.lock().binding.is_some() {
                    return;
                }
                changed.await;
            }
        };
        tokio::select! {
            biased;
            _ = stopping(&server) => { self.shutdown_observed(); return; }
            _ = ready => {}
        }
        let active = async {
            let _relationship = OwnerRelationship {
                owner: &self,
                epoch: 1,
            };
            self.active(&mut bootstrap, 1).await;
        };
        tokio::pin!(active);
        tokio::select! {
            _ = &mut active => self.watch_deadline().await,
            _ = self.watch_deadline() => active.await,
        }
    }
}

impl OwnerControlHandler for OwnerSupervisor {
    fn bound(&self, binding: EndpointContextBinding) -> std::io::Result<()> {
        // Bind captured this identity under its native namespace lock. Moving
        // that snapshot into Core neither grants a capability nor reopens it.
        // Every command that issues or uses a grant validates the current pins.
        self.lifecycle.state.lock().binding = Some(binding);
        self.lifecycle.changed.notify_waiters();
        Ok(())
    }

    fn setup_deadline(&self, started: Instant) -> Instant {
        let setup = started + c2_local::DEFAULT_CONNECT_TIMEOUT;
        self.lifecycle
            .state
            .lock()
            .deadline
            .map_or(setup, |grace| setup.min(grace))
    }

    fn serve(
        self: Arc<Self>,
        session: OwnerSession,
        first: OwnerMessage,
        deadline: Instant,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(self.candidate(session, first, deadline))
    }
}

struct OwnerRelationship<'a> {
    owner: &'a OwnerSupervisor,
    epoch: u64,
}
impl Drop for OwnerRelationship<'_> {
    fn drop(&mut self) {
        self.owner.ended(self.epoch, None);
    }
}

fn peer_ended(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
    )
}

async fn stopping(server: &Server) {
    tokio::select! {
        _ = server.wait_for_shutdown_requested() => {},
        _ = server.wait_until_terminal() => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_error_diagnosis_survives_completion() {
        let cell = HostLifecycleCell::new(&ServerLifecyclePolicy::Persistent);
        cell.state.lock().phase = HostLifecyclePhase::WatcherIoError("invalid control".into());
        cell.finish_from_outcome(&crate::ShutdownOutcome::default());
        assert_eq!(
            cell.phase(),
            HostLifecyclePhase::WatcherIoError("invalid control".into())
        );
    }

    #[test]
    fn preclosed_bootstrap_is_refused_before_ready() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (controller, mut bootstrap) = c2_local::owner_endpoint_pair().unwrap();
        drop(controller);
        assert!(matches!(
            runtime.block_on(arm_owner_watch(&mut bootstrap)),
            OwnerArmOutcome::Closed
        ));
    }
}
