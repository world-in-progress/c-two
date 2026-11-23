use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use c2_contract::{
    ContractError, ContractRelease, ContractReleaseRef, ExpectedRouteContract,
    MAX_CONTRACT_METHODS, MAX_CONTRACT_TOPICS, MethodAccess, MethodMode, PORTABLE_CONTRACT_SCHEMA,
    ValidatedContractDescriptor, validate_contract_text_field, validate_expected_route_contract,
};
use c2_error::{C2Error, ErrorCode};
use c2_mem::{RecycledBytes, WritableSlab};
use c2_server::{
    AccessLevel, ConcurrencyMode, CrmCallback, CrmError, CrmStream, CrmStreamCallback,
    CrmStreamItem, ResponseMeta, RouteBuildSpec, RouteConcurrencyHandle, SchedulerAcquireError,
    SchedulerGuard, SchedulerLimits, SchedulerSnapshot, Server, ServerFailure, ServerIdentity,
    ServerRuntimeBuilder,
};

use crate::topic::{
    LocalTopicSubscription, RouteTopics, SubscribeOptions, TopicCloseCause, TopicConfig,
    TopicPublisher,
};
use crate::{
    CoreLocalResponseStream, Error, FailedStartShutdownReceipt, LifecycleError, LocalObjectStream,
    RegisterOutcome, Runtime, ShutdownOutcome, UnregisterOutcome,
};

/// One generated method entry validated against an admitted release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDefinition {
    pub index: u16,
    pub name: String,
    pub access: MethodAccess,
}

/// Allocation access to the response pool advertised by this Host.
pub struct ResponseSink {
    pool: Arc<c2_server::ResponsePool>,
}

impl ResponseSink {
    /// Try to allocate a writable response backing above the inline threshold.
    pub fn try_alloc_slab(&self, len: usize) -> Result<Option<WritableSlab>, String> {
        self.pool.try_alloc_slab(c2_config::SHM_THRESHOLD, len)
    }

    /// Seal a filled slab allocated from this sink into an encoded SHM response.
    pub fn finish_slab(&self, slab: WritableSlab) -> Result<EncodedResponse, String> {
        self.pool.finish_slab(slab).map(EncodedResponse::Shm)
    }
}

/// Encoded response with either owned bytes or an unpublished SHM allocation.
pub enum EncodedResponse {
    Inline(Vec<u8>),
    Shm(c2_server::PreparedShmResponse),
}

impl EncodedResponse {
    fn into_meta(self) -> ResponseMeta {
        match self {
            Self::Inline(bytes) => ResponseMeta::Inline(bytes),
            Self::Shm(prepared) => ResponseMeta::ShmAlloc(prepared),
        }
    }
}

/// Opaque-byte service boundary implemented by generated host adapters.
///
/// The service receives an owned local copy, independent of transport storage.
/// Transport storage has already been released before invocation; adapters
/// invalidate callback-scoped payload owners before dropping the local bytes.
pub trait EncodedService: Send + Sync + 'static {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error>;

    fn invoke_into(
        &self,
        method_index: u16,
        request: RecycledBytes,
        _sink: &ResponseSink,
    ) -> Result<EncodedResponse, C2Error> {
        self.invoke(method_index, request.into_vec())
            .map(EncodedResponse::Inline)
    }
}

/// A resource-owned item whose encoding is deferred until storage admission.
pub trait EncodedStreamItem: Send + 'static {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Encode the requested range directly into the selected transport storage.
    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), C2Error>;
}

/// The resource continuation. Establishment, steps and finish run serially
/// under the original route admission; only finish destroys its iterator.
pub trait EncodedStream: Send + 'static {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error>;
    fn finish(self: Box<Self>) -> Result<(), C2Error>;
}

pub trait EncodedStreamService: EncodedService {
    fn open_stream(
        &self,
        method_index: u16,
        request: Vec<u8>,
    ) -> Result<Box<dyn EncodedStream>, C2Error>;

    fn open_stream_recycled(
        &self,
        method_index: u16,
        request: RecycledBytes,
    ) -> Result<Box<dyn EncodedStream>, C2Error> {
        self.open_stream(method_index, request.into_vec())
    }
}

/// Language-neutral execution policy shared by remote dispatch and any
/// same-process SDK projection of a registered route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ServiceConcurrencyMode {
    Parallel,
    Exclusive,
    #[default]
    ReadParallel,
}

impl ServiceConcurrencyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parallel => "parallel",
            Self::Exclusive => "exclusive",
            Self::ReadParallel => "read_parallel",
        }
    }
}

impl From<ServiceConcurrencyMode> for ConcurrencyMode {
    fn from(mode: ServiceConcurrencyMode) -> Self {
        match mode {
            ServiceConcurrencyMode::Parallel => Self::Parallel,
            ServiceConcurrencyMode::Exclusive => Self::Exclusive,
            ServiceConcurrencyMode::ReadParallel => Self::ReadParallel,
        }
    }
}

impl From<ConcurrencyMode> for ServiceConcurrencyMode {
    fn from(mode: ConcurrencyMode) -> Self {
        match mode {
            ConcurrencyMode::Parallel => Self::Parallel,
            ConcurrencyMode::Exclusive => Self::Exclusive,
            ConcurrencyMode::ReadParallel => Self::ReadParallel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteConcurrencySnapshot {
    pub mode: ServiceConcurrencyMode,
    pub max_pending: Option<usize>,
    pub max_workers: Option<usize>,
    pub pending: usize,
    pub active_workers: usize,
    pub closed: bool,
    pub is_unconstrained: bool,
}

impl From<SchedulerSnapshot> for RouteConcurrencySnapshot {
    fn from(snapshot: SchedulerSnapshot) -> Self {
        Self {
            mode: snapshot.mode.into(),
            max_pending: snapshot.max_pending,
            max_workers: snapshot.max_workers,
            pending: snapshot.pending,
            active_workers: snapshot.active_workers,
            closed: snapshot.closed,
            is_unconstrained: snapshot.is_unconstrained,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteConcurrencyError {
    Closed,
    Unavailable,
    Capacity {
        field: &'static str,
        limit: usize,
    },
    /// The owning Host's server domain has no free operation slot (or its
    /// memory budget is zero): the same-process direct call never entered the
    /// resource method. `limit` is the domain's outstanding-call bound.
    DomainAdmission {
        limit: u64,
        used: u64,
    },
}

impl fmt::Display for RouteConcurrencyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("route closed"),
            Self::Unavailable => formatter.write_str("host admission unavailable"),
            Self::Capacity { field, limit } => {
                write!(
                    formatter,
                    "route concurrency capacity exceeded: {field}={limit}"
                )
            }
            Self::DomainAdmission { limit, used } => write!(
                formatter,
                "server domain call admission refused: {used} of {limit} slots in use"
            ),
        }
    }
}

impl std::error::Error for RouteConcurrencyError {}

impl From<SchedulerAcquireError> for RouteConcurrencyError {
    fn from(error: SchedulerAcquireError) -> Self {
        match error {
            SchedulerAcquireError::Closed => Self::Closed,
            SchedulerAcquireError::Unavailable => Self::Unavailable,
            SchedulerAcquireError::Capacity { field, limit } => Self::Capacity { field, limit },
        }
    }
}

/// Cloneable projection of the exact scheduler used by remote route dispatch.
#[derive(Clone)]
pub struct RouteConcurrency {
    inner: RouteConcurrencyHandle,
    method_modes: Arc<[MethodMode]>,
    /// The owning Host's server domain. Every same-process direct call admits
    /// through one of its operation slots exactly like a remote call, without
    /// touching the client domain.
    server_budget: c2_mem::DomainBudget,
}

impl fmt::Debug for RouteConcurrency {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteConcurrency")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl RouteConcurrency {
    pub fn snapshot(&self) -> RouteConcurrencySnapshot {
        self.inner.snapshot().into()
    }

    /// Reserve one server-domain operation slot immediately, then the route's
    /// own scheduler admission. The slot is granted or refused on the spot;
    /// the direct call holds it until its guard drops (the call returns).
    fn reserve_direct_slot(&self) -> Result<c2_mem::OperationPermit, RouteConcurrencyError> {
        self.server_budget.reserve_operation().map_err(|error| {
            RouteConcurrencyError::DomainAdmission {
                limit: error.max_operations,
                used: error.used_operations,
            }
        })
    }

    pub fn blocking_acquire(
        &self,
        method_index: u16,
    ) -> Result<RouteConcurrencyGuard, RouteConcurrencyError> {
        let _permit = self.reserve_direct_slot()?;
        match self.inner.blocking_acquire(method_index) {
            Ok(inner) => Ok(RouteConcurrencyGuard { inner, _permit }),
            Err(error) => Err(RouteConcurrencyError::from(error)),
        }
    }

    pub fn is_unconstrained(&self) -> bool {
        self.inner.is_unconstrained()
    }

    /// Start a same-process object stream on the registered route's own call
    /// slot. The factory and every continuation run only during a route step.
    pub fn open_local_stream<T, F>(
        &self,
        method_index: u16,
        factory: F,
    ) -> Result<CoreLocalResponseStream<T>, Error>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<Box<dyn LocalObjectStream<T>>, C2Error> + Send + 'static,
    {
        if self.method_modes.get(method_index as usize) != Some(&MethodMode::ServerStream) {
            return Err(C2Error::new(
                c2_error::ErrorCode::ContractMismatch,
                "method is not a registered server stream",
            )
            .into());
        }
        let permit = self.reserve_direct_slot().map_err(C2Error::from)?;
        let call = self
            .inner
            .begin_local_stream_call(method_index)
            .map_err(local_stream_admission_error)?;
        CoreLocalResponseStream::open(
            call,
            factory,
            permit,
            #[cfg(test)]
            None,
        )
    }
}

/// The server domain refused a same-process direct call before the resource
/// method ran: canonical 717 with the call-execution admission stage.
impl From<RouteConcurrencyError> for C2Error {
    fn from(error: RouteConcurrencyError) -> Self {
        match error {
            RouteConcurrencyError::DomainAdmission { limit, used } => C2Error::new(
                c2_error::ErrorCode::CallCapacityExceeded,
                format!("server domain call admission refused: {used} of {limit} slots in use"),
            )
            .with_details(std::collections::BTreeMap::from([
                ("stage".to_string(), "call_execution_admission".to_string()),
                ("dispatch_phase".to_string(), "pre_dispatch".to_string()),
            ])),
            RouteConcurrencyError::Closed => {
                local_stream_scheduler_error(SchedulerAcquireError::Closed)
            }
            RouteConcurrencyError::Unavailable => {
                local_stream_scheduler_error(SchedulerAcquireError::Unavailable)
            }
            RouteConcurrencyError::Capacity { field, limit } => {
                local_stream_scheduler_error(SchedulerAcquireError::Capacity { field, limit })
            }
        }
    }
}

pub(crate) fn local_stream_admission_error(error: SchedulerAcquireError) -> Error {
    local_stream_scheduler_error(error).into()
}

pub(crate) fn local_stream_scheduler_error(error: SchedulerAcquireError) -> C2Error {
    let (code, message) = match error {
        SchedulerAcquireError::Closed => (
            c2_error::ErrorCode::ResourceClosed,
            "route closed".to_string(),
        ),
        SchedulerAcquireError::Unavailable => (
            c2_error::ErrorCode::ResourceUnavailable,
            "host admission unavailable".to_string(),
        ),
        SchedulerAcquireError::Capacity { field, limit } => (
            c2_error::ErrorCode::ResourceUnavailable,
            format!("route concurrency capacity exceeded: {field}={limit}"),
        ),
    };
    C2Error::new(code, message)
}

pub struct RouteConcurrencyGuard {
    inner: SchedulerGuard,
    /// The direct call's server-domain slot, held until this guard drops.
    _permit: c2_mem::OperationPermit,
}

impl RouteConcurrencyGuard {
    /// Final native admission after all SDK-side waiting, immediately before
    /// entering the resource callback. Already committed work may drain.
    pub fn commit(&mut self) -> Result<(), RouteConcurrencyError> {
        self.inner.commit().map_err(RouteConcurrencyError::from)
    }
}

impl fmt::Debug for RouteConcurrencyGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteConcurrencyGuard")
            .finish_non_exhaustive()
    }
}

/// A release-verified route definition ready for Core registration.
pub struct ServiceDefinition {
    release_ref: ContractReleaseRef,
    expected: ExpectedRouteContract,
    methods: Arc<[MethodDefinition]>,
    topics: BTreeMap<String, TopicConfig>,
    initial_topics: BTreeMap<String, Vec<u8>>,
    service: Arc<dyn EncodedService>,
    stream_service: Option<Arc<dyn EncodedStreamService>>,
    stream_descriptor: Option<ValidatedContractDescriptor>,
    stream_modes: Option<Arc<[MethodMode]>>,
    concurrency_mode: ServiceConcurrencyMode,
    max_pending: Option<usize>,
    max_workers: Option<usize>,
}

impl fmt::Debug for ServiceDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceDefinition")
            .field("release_ref", &self.release_ref)
            .field("expected", &self.expected)
            .field("methods", &self.methods)
            .field("concurrency_mode", &self.concurrency_mode)
            .field("max_pending", &self.max_pending)
            .field("max_workers", &self.max_workers)
            .finish_non_exhaustive()
    }
}

impl ServiceDefinition {
    pub fn new<I>(
        release: &ContractRelease,
        release_ref: ContractReleaseRef,
        route_name: impl Into<String>,
        methods: I,
        service: Arc<dyn EncodedService>,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = MethodDefinition>,
    {
        Self::from_release(
            release,
            release_ref,
            route_name.into(),
            methods,
            service,
            None,
        )
    }

    pub fn new_stream<I>(
        release: &ContractRelease,
        release_ref: ContractReleaseRef,
        route_name: impl Into<String>,
        methods: I,
        service: Arc<dyn EncodedStreamService>,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = MethodDefinition>,
    {
        Self::from_release(
            release,
            release_ref,
            route_name.into(),
            methods,
            service.clone(),
            Some(service),
        )
    }

    fn from_release<I>(
        release: &ContractRelease,
        release_ref: ContractReleaseRef,
        route_name: String,
        methods: I,
        service: Arc<dyn EncodedService>,
        stream_service: Option<Arc<dyn EncodedStreamService>>,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = MethodDefinition>,
    {
        release_ref.verify_release(release)?;
        let expected = release.expected_route(route_name)?;
        let methods = methods.into_iter().collect::<Vec<_>>();
        let descriptor_methods = release.descriptor().methods();
        if stream_service.is_none()
            && descriptor_methods
                .iter()
                .any(|method| method.mode() == MethodMode::ServerStream)
        {
            return Err(method_contract_error(
                "$.methods",
                "server-stream methods require a stream service".into(),
            ));
        }
        if methods.len() != descriptor_methods.len() {
            return Err(method_contract_error(
                "$.methods",
                format!(
                    "service defines {} methods but release defines {}",
                    methods.len(),
                    descriptor_methods.len()
                ),
            ));
        }
        for (position, (method, descriptor)) in
            methods.iter().zip(descriptor_methods.iter()).enumerate()
        {
            let expected_index = u16::try_from(position).map_err(|_| {
                method_contract_error(
                    "$.methods",
                    "method index exceeds the C-Two wire capacity".to_string(),
                )
            })?;
            if method.index != expected_index {
                return Err(method_contract_error(
                    &format!("$.methods[{position}].index"),
                    format!(
                        "expected positional index {expected_index}, got {}",
                        method.index
                    ),
                ));
            }
            if method.name != descriptor.name() {
                return Err(method_contract_error(
                    &format!("$.methods[{position}].name"),
                    format!(
                        "expected release method {:?}, got {:?}",
                        descriptor.name(),
                        method.name
                    ),
                ));
            }
            if method.access != descriptor.access() {
                return Err(method_contract_error(
                    &format!("$.methods[{position}].access"),
                    format!(
                        "expected release access {:?}, got {:?}",
                        descriptor.access(),
                        method.access
                    ),
                ));
            }
        }

        let mut topics = BTreeMap::new();
        for topic in release.descriptor().topics() {
            topics.insert(topic.name().to_string(), TopicConfig::default());
        }

        Ok(Self {
            release_ref,
            expected,
            methods: methods.into(),
            topics,
            initial_topics: BTreeMap::new(),
            service,
            stream_descriptor: stream_service
                .as_ref()
                .map(|_| release.descriptor().clone()),
            stream_modes: stream_service.as_ref().map(|_| {
                release
                    .descriptor()
                    .methods()
                    .iter()
                    .map(|method| method.mode())
                    .collect::<Vec<_>>()
                    .into()
            }),
            stream_service,
            concurrency_mode: ServiceConcurrencyMode::default(),
            max_pending: None,
            max_workers: None,
        })
    }

    /// Construct an explicitly nonportable language service.
    ///
    /// This path exists for language-local behaviors such as Python pickle. It
    /// requires a release reference whose schema is not the portable
    /// `c-two.contract.v2` schema and therefore cannot masquerade as a portable
    /// generated service.
    #[doc(hidden)]
    pub fn new_nonportable<I>(
        release_ref: ContractReleaseRef,
        expected: ExpectedRouteContract,
        methods: I,
        service: Arc<dyn EncodedService>,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = MethodDefinition>,
    {
        validate_expected_route_contract(&expected)?;
        if release_ref.contract_schema() == PORTABLE_CONTRACT_SCHEMA {
            return Err(method_contract_error(
                "$.contract_schema",
                "portable services must be constructed from an admitted ContractRelease"
                    .to_string(),
            ));
        }
        for (path, reference, route) in [
            (
                "$.crm.namespace",
                release_ref.crm_namespace(),
                expected.crm_ns.as_str(),
            ),
            (
                "$.crm.name",
                release_ref.crm_name(),
                expected.crm_name.as_str(),
            ),
            (
                "$.crm.version",
                release_ref.crm_version(),
                expected.crm_ver.as_str(),
            ),
        ] {
            if reference != route {
                return Err(method_contract_error(
                    path,
                    format!(
                        "nonportable release reference value {reference:?} does not match route value {route:?}"
                    ),
                ));
            }
        }

        let methods = methods.into_iter().collect::<Vec<_>>();
        validate_nonportable_methods(&methods)?;
        Ok(Self {
            release_ref,
            expected,
            methods: methods.into(),
            topics: BTreeMap::new(),
            initial_topics: BTreeMap::new(),
            service,
            stream_service: None,
            stream_descriptor: None,
            stream_modes: None,
            concurrency_mode: ServiceConcurrencyMode::default(),
            max_pending: None,
            max_workers: None,
        })
    }

    /// Construct a language-local stream service. The language descriptor
    /// supplies the already-validated positional method modes.
    #[doc(hidden)]
    pub fn new_nonportable_stream<I, M>(
        release_ref: ContractReleaseRef,
        expected: ExpectedRouteContract,
        methods: I,
        method_modes: M,
        service: Arc<dyn EncodedStreamService>,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = MethodDefinition>,
        M: IntoIterator<Item = MethodMode>,
    {
        let mut definition =
            Self::new_nonportable(release_ref, expected, methods, service.clone())?;
        let modes = method_modes.into_iter().collect::<Vec<_>>();
        if modes.len() != definition.methods.len()
            || !modes.iter().any(|mode| *mode == MethodMode::ServerStream)
        {
            return Err(method_contract_error(
                "$.methods",
                "stream modes must cover every method and include a server stream".into(),
            ));
        }
        definition.stream_service = Some(service);
        definition.stream_modes = Some(modes.into());
        Ok(definition)
    }

    pub fn with_concurrency(
        mut self,
        mode: ServiceConcurrencyMode,
        max_pending: Option<usize>,
        max_workers: Option<usize>,
    ) -> Result<Self, Error> {
        SchedulerLimits::try_from_usize(max_pending, max_workers)
            .map_err(|message| method_contract_error("$.concurrency", message))?;
        self.concurrency_mode = mode;
        self.max_pending = max_pending;
        self.max_workers = max_workers;
        Ok(self)
    }

    /// Declare nonportable language-local topic names on a nonportable service.
    #[doc(hidden)]
    pub fn with_nonportable_topics<I, S>(mut self, topic_names: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if self.release_ref.contract_schema() == PORTABLE_CONTRACT_SCHEMA {
            return Err(method_contract_error(
                "$.topics",
                "portable service topics come from the admitted ContractRelease".into(),
            ));
        }
        let method_names: HashSet<&str> = self.methods.iter().map(|m| m.name.as_str()).collect();
        let mut topics = BTreeMap::new();
        for (index, raw_name) in topic_names.into_iter().enumerate() {
            let name = raw_name.into();
            let path = format!("$.topics[{index}].name");
            validate_contract_text_field("topic name", &name)
                .map_err(|error| method_contract_error(&path, error.to_string()))?;
            if method_names.contains(name.as_str()) {
                return Err(method_contract_error(
                    &path,
                    format!("topic name {name:?} collides with a method name"),
                ));
            }
            if topics.insert(name.clone(), TopicConfig::default()).is_some() {
                return Err(method_contract_error(
                    &path,
                    format!("duplicate topic name {name:?}"),
                ));
            }
            if topics.len() > MAX_CONTRACT_TOPICS {
                return Err(method_contract_error(
                    "$.topics",
                    format!("topic count exceeds hard limit {MAX_CONTRACT_TOPICS}"),
                ));
            }
        }
        self.topics = topics;
        Ok(self)
    }

    /// Configure the bounded history ring depth (`>= 1`) of a declared topic.
    pub fn with_topic_capacity(
        mut self,
        topic_name: &str,
        capacity: usize,
    ) -> Result<Self, Error> {
        let config = TopicConfig { capacity };
        config.validate().map_err(Error::Semantic)?;
        let slot = self.topics.get_mut(topic_name).ok_or_else(|| {
            Error::Semantic(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unknown topic {topic_name:?} on contract"),
            ))
        })?;
        *slot = config;
        Ok(self)
    }

    /// Stage an initial published payload for a declared topic. Committed
    /// atomically before the route opens and rolled back if registration fails.
    pub fn with_initial_topic_bytes(
        mut self,
        topic_name: &str,
        payload: Vec<u8>,
    ) -> Result<Self, Error> {
        if !self.topics.contains_key(topic_name) {
            return Err(Error::Semantic(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unknown topic {topic_name:?} on contract"),
            )));
        }
        if self
            .initial_topics
            .insert(topic_name.to_string(), payload)
            .is_some()
        {
            return Err(Error::Semantic(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("duplicate initial payload for topic {topic_name:?}"),
            )));
        }
        Ok(self)
    }

    pub fn topic_names(&self) -> Vec<&str> {
        self.topics.keys().map(String::as_str).collect()
    }

    pub fn release_ref(&self) -> &ContractReleaseRef {
        &self.release_ref
    }

    pub fn expected_route(&self) -> &ExpectedRouteContract {
        &self.expected
    }

    pub fn methods(&self) -> &[MethodDefinition] {
        &self.methods
    }
}

fn validate_nonportable_methods(methods: &[MethodDefinition]) -> Result<(), Error> {
    if methods.len() > MAX_CONTRACT_METHODS {
        return Err(method_contract_error(
            "$.methods",
            format!(
                "service defines {} methods but the contract cap is {MAX_CONTRACT_METHODS}",
                methods.len()
            ),
        ));
    }
    let mut names = HashSet::new();
    for (position, method) in methods.iter().enumerate() {
        let expected_index = u16::try_from(position).map_err(|_| {
            method_contract_error(
                "$.methods",
                "method index exceeds the C-Two wire capacity".to_string(),
            )
        })?;
        if method.index != expected_index {
            return Err(method_contract_error(
                &format!("$.methods[{position}].index"),
                format!(
                    "expected positional index {expected_index}, got {}",
                    method.index
                ),
            ));
        }
        validate_contract_text_field("method name", &method.name)?;
        if !names.insert(method.name.as_str()) {
            return Err(method_contract_error(
                &format!("$.methods[{position}].name"),
                format!("duplicate method name {:?}", method.name),
            ));
        }
    }
    Ok(())
}

fn method_contract_error(path: &str, message: String) -> Error {
    ContractError::InvalidDescriptor {
        path: path.to_string(),
        message,
    }
    .into()
}

#[derive(Debug, Clone, Default)]
enum RelaySelection {
    #[default]
    Inherit,
    Disabled,
    Explicit(String),
}

/// Core host lifecycle options: the complete Host identity, start/stop
/// budgets, carrying capacity and server-domain limits, plus relay selection.
/// The server fields are a tri-state patch over the Runtime's accepted server
/// overrides; every field defaults to `Keep`.
#[derive(Debug, Clone, Default)]
pub struct HostOptions {
    pub server: c2_config::ServerConfigPatch,
    relay: RelaySelection,
    relay_use_proxy: Option<bool>,
    shutdown_timeout: Option<Duration>,
    #[cfg(test)]
    startup_gate: Option<Arc<StartupGateForTest>>,
}

#[cfg(test)]
#[derive(Debug)]
struct StartupGateForTest {
    entered: std::sync::mpsc::SyncSender<Arc<Server>>,
    proceed: Mutex<std::sync::mpsc::Receiver<()>>,
    thread_join: Option<std::sync::mpsc::SyncSender<std::thread::JoinHandle<()>>>,
    fail_spawn: bool,
    fail_wait_runtime: bool,
}

impl HostOptions {
    pub fn with_relay_anchor_address(mut self, address: impl Into<String>) -> Self {
        self.relay = RelaySelection::Explicit(address.into());
        self
    }

    pub fn without_relay(mut self) -> Self {
        self.relay = RelaySelection::Disabled;
        self
    }

    pub fn with_relay_use_proxy(mut self, use_proxy: bool) -> Self {
        self.relay_use_proxy = Some(use_proxy);
        self
    }

    /// Explicit server identity for this Host and every later start of the
    /// same frozen server role.
    pub fn with_server_id(mut self, server_id: impl Into<String>) -> Self {
        self.server.server_id = c2_config::ConfigField::Set(server_id.into());
        self
    }

    /// Override this Host's startup wait budget. Otherwise the accepted
    /// server overrides apply, falling back to a finite five-second default.
    /// Native cleanup after expiry retains its separate shutdown budget.
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.server.startup_timeout = c2_config::ConfigField::Set(timeout);
        self
    }

    /// Override the Host's single-message carrying bound.
    pub fn with_max_payload_size(mut self, max_payload_size: u64) -> Self {
        self.server.max_payload_size = c2_config::ConfigField::Set(max_payload_size);
        self
    }

    /// Override the Host's execution worker bound.
    pub fn with_max_execution_workers(mut self, max_execution_workers: u32) -> Self {
        self.server.max_execution_workers = c2_config::ConfigField::Set(max_execution_workers);
        self
    }

    /// Limit the number of settled call outcomes retained across this Host's registrations.
    pub fn with_outcome_max_entries(mut self, max_entries: usize) -> Self {
        self.server.outcome_max_entries = c2_config::ConfigField::Set(max_entries);
        self
    }

    /// Limit retained call result and error bytes. Zero retains only outcome metadata.
    pub fn with_outcome_max_retained_bytes(mut self, bytes: u64) -> Self {
        self.server.outcome_max_retained_bytes = c2_config::ConfigField::Set(bytes);
        self
    }

    /// Override the server domain's memory budget.
    pub fn with_memory_budget_bytes(mut self, bytes: u64) -> Self {
        self.server.domain_limits.memory_budget_bytes = c2_config::ConfigField::Set(bytes);
        self
    }

    /// Override the server domain's file-spill budget.
    pub fn with_file_spill_budget_bytes(mut self, bytes: u64) -> Self {
        self.server.domain_limits.file_spill_budget_bytes = c2_config::ConfigField::Set(bytes);
        self
    }

    /// Override the server domain's outstanding-call bound.
    pub fn with_max_outstanding_calls(mut self, calls: u64) -> Self {
        self.server.domain_limits.max_outstanding_calls = c2_config::ConfigField::Set(calls);
        self
    }

    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = Some(timeout);
        self
    }

    pub(crate) fn resolve_settings(
        &self,
        catalog: &c2_config::EnvCatalog,
        frozen: Option<&crate::roles::HostSettings>,
        inherited_anchor: impl FnOnce() -> Result<Option<String>, LifecycleError>,
    ) -> Result<crate::roles::HostSettings, LifecycleError> {
        let relay_anchor_address = match &self.relay {
            RelaySelection::Inherit => match frozen {
                Some(settings) => settings.relay_anchor_address.clone(),
                None => inherited_anchor()?,
            },
            RelaySelection::Disabled => None,
            RelaySelection::Explicit(address) => {
                Some(crate::session::canonical_relay_anchor_address(address))
            }
        };
        if relay_anchor_address
            .as_deref()
            .is_some_and(|anchor| !c2_http::valid_relay_url(anchor))
        {
            return Err(LifecycleError::Relay("invalid HTTP relay base URL".into()));
        }
        let relay_use_proxy = match (self.relay_use_proxy, frozen) {
            (_, _) if relay_anchor_address.is_none() => false,
            (Some(use_proxy), _) => use_proxy,
            (None, Some(settings)) => settings.relay_use_proxy,
            (None, None) => {
                c2_config::ConfigResolver::resolve_relay_client(None, catalog)
                    .map_err(|error| LifecycleError::Configuration(error.to_string()))?
                    .use_proxy
            }
        };
        let shutdown_timeout = self
            .shutdown_timeout
            .or_else(|| frozen.map(|settings| settings.shutdown_timeout))
            .unwrap_or(Duration::from_secs(5));
        c2_config::deadline_after(Instant::now(), shutdown_timeout)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        Ok(crate::roles::HostSettings {
            relay_anchor_address,
            relay_use_proxy,
            shutdown_timeout,
        })
    }
}

/// Core-owned host for one process server and any number of validated routes.
#[derive(Clone)]
pub struct Host {
    inner: Arc<HostInner>,
}

impl fmt::Debug for Host {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Host")
            .field("server_id", &self.inner.server.server_id())
            .field("ipc_address", &self.inner.server.local_endpoint().address())
            .finish_non_exhaustive()
    }
}

pub(crate) struct HostInner {
    runtime: Runtime,
    server: Arc<Server>,
    /// The Runtime's server role domain; same-process direct calls admit
    /// through its operation slots exactly like remote calls.
    domain_budget: c2_mem::DomainBudget,
    relay_anchor_address: Option<String>,
    relay_use_proxy: bool,
    shutdown_timeout: Duration,
    route_names: Mutex<HashSet<String>>,
    route_topics: Mutex<HashMap<String, Arc<RouteTopics>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown_outcome: Mutex<Option<ShutdownOutcome>>,
    shutdown_gate: Mutex<()>,
    lifecycle: crate::lifecycle::HostLifecycleCell,
}

impl HostInner {
    fn close_all_topics(&self, cause: TopicCloseCause) {
        let topics = std::mem::take(&mut *self.route_topics.lock());
        for (_, route_topics) in topics {
            route_topics.close(cause);
        }
    }

    fn refresh_shutdown_outcome(&self) {
        if self.shutdown_outcome.lock().is_none() {
            if let Some(outcome) = self.runtime.shutdown_observation(&self.server) {
                self.close_all_topics(TopicCloseCause::HostShutdown);
                self.lifecycle.finish_from_outcome(&outcome);
                *self.shutdown_outcome.lock() = Some(outcome);
                self.route_names.lock().clear();
            }
        }
    }

    fn shutdown(&self) -> ShutdownOutcome {
        self.shutdown_with_timeout(self.shutdown_timeout)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> ShutdownOutcome {
        let deadline = match c2_config::deadline_after(Instant::now(), timeout) {
            Ok(deadline) => deadline,
            Err(error) => return ShutdownOutcome::invalid_timeout(error),
        };
        self.refresh_shutdown_outcome();
        if let Some(outcome) = self.shutdown_outcome.lock().clone() {
            return outcome;
        }
        let Some(_gate) = self
            .shutdown_gate
            .try_lock_for(deadline.saturating_duration_since(Instant::now()))
        else {
            return ShutdownOutcome {
                runtime_barrier_error: Some("native shutdown transaction is still pending".into()),
                ..Default::default()
            };
        };
        self.run_shutdown_transaction(deadline.saturating_duration_since(Instant::now()))
    }

    fn run_shutdown_transaction(&self, timeout: Duration) -> ShutdownOutcome {
        self.refresh_shutdown_outcome();
        if let Some(outcome) = self.shutdown_outcome.lock().clone() {
            return outcome;
        }
        let route_names = self.route_names.lock().iter().cloned().collect();
        self.close_all_topics(TopicCloseCause::HostShutdown);
        let outcome = self.runtime.shutdown(
            Some(&self.server),
            route_names,
            self.relay_anchor_address.as_deref(),
            self.relay_use_proxy,
            None,
            timeout,
        );
        // Never join a running drain or the current host thread. Dropping a JoinHandle
        // detaches it; its Server/runtime continue owning all actual callback work.
        if self
            .thread
            .lock()
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            if let Some(thread) = self.thread.lock().take() {
                let _ = thread.join();
            }
        }
        if outcome.runtime_barrier_error.is_none() && outcome.route_close_error.is_none() {
            self.route_names.lock().clear();
            self.lifecycle.finish_from_outcome(&outcome);
            *self.shutdown_outcome.lock() = Some(outcome.clone());
        }
        outcome
    }

    fn begin_failed_start_shutdown(&self) -> FailedStartShutdownReceipt {
        // The Server shutdown request and its own wait already ran. Start the
        // original Core transaction here, before a host thread can postpone
        // HostInner::drop by upgrading its Weak reference.
        self.runtime.begin_failed_start_shutdown(
            &self.server,
            self.route_names.lock().iter().cloned().collect(),
            self.relay_anchor_address.as_deref(),
            self.relay_use_proxy,
            self.shutdown_timeout,
        )
    }

    fn lifecycle_snapshot(&self) -> crate::lifecycle::HostLifecycleSnapshot {
        self.refresh_shutdown_outcome();
        let server_state = self.server.lifecycle_state();
        let work_drained = self.shutdown_outcome.lock().as_ref().map(|outcome| {
            outcome
                .route_outcomes
                .iter()
                .all(|close| close.active_drained)
        });
        crate::lifecycle::HostLifecycleSnapshot {
            finished: self.lifecycle.finished(),
            listener_closed: matches!(
                server_state,
                c2_server::ServerLifecycleState::Stopped
                    | c2_server::ServerLifecycleState::Failed(_)
            ),
            work_drained,
        }
    }

    fn unregister(
        &self,
        route_name: &str,
        topics: &Arc<RouteTopics>,
    ) -> Result<UnregisterOutcome, Error> {
        self.refresh_shutdown_outcome();
        if self.runtime.shutdown_pending(&self.server) {
            return Err(LifecycleError::Server(ServerFailure::new(
                "native shutdown is still draining; route hooks are not yet safe".into(),
            ))
            .into());
        }
        if let Some(shutdown) = self.shutdown_outcome.lock().as_ref()
            && let Some(close) = shutdown
                .route_outcomes
                .iter()
                .find(|close| close.route_name == route_name)
        {
            topics.close(TopicCloseCause::HostShutdown);
            return Ok(UnregisterOutcome {
                route_name: route_name.to_string(),
                local_removed: close.local_removed,
                close: close.clone(),
                relay_error: shutdown
                    .relay_errors
                    .iter()
                    .find(|error| error.route_name == route_name)
                    .cloned(),
            });
        }
        let outcome = match self.runtime.unregister_route(
            &self.server,
            route_name,
            Some(topics),
            self.relay_anchor_address.as_deref(),
            self.relay_use_proxy,
        ) {
            Ok(outcome) => outcome,
            Err(LifecycleError::MissingRoute(_)) => UnregisterOutcome {
                route_name: route_name.to_string(),
                local_removed: false,
                close: crate::RouteCloseOutcome {
                    route_name: route_name.to_string(),
                    local_removed: false,
                    active_drained: true,
                    closed_reason: "replaced".into(),
                    close_error: None,
                },
                relay_error: None,
            },
            Err(error) => return Err(error.into()),
        };
        let mut current_topics = self.route_topics.lock();
        if current_topics
            .get(route_name)
            .is_some_and(|current| Arc::ptr_eq(current, topics))
        {
            current_topics.remove(route_name);
            self.route_names.lock().remove(route_name);
        }
        Ok(outcome)
    }
}

impl Drop for HostInner {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl Host {
    pub(crate) fn inner(&self) -> &Arc<HostInner> {
        &self.inner
    }

    pub(crate) fn from_inner(inner: Arc<HostInner>) -> Self {
        Self { inner }
    }

    pub(crate) fn belongs_to(&self, runtime: &Runtime) -> bool {
        self.inner.runtime.same_instance(runtime)
    }

    pub(crate) fn seal_all_topics_for_shutdown(&self) {
        self.inner.close_all_topics(TopicCloseCause::HostShutdown);
    }

    pub(crate) fn retirement_server(&self) -> std::sync::Weak<Server> {
        Arc::downgrade(&self.inner.server)
    }

    pub(crate) fn retirement_work(
        &self,
        deadline: Instant,
    ) -> Result<crate::session::ShutdownWork, LifecycleError> {
        let routes = self.inner.route_names.try_lock_until(deadline).ok_or(
            LifecycleError::ShutdownWaitTimeout {
                stage: "core_host_routes_wait",
            },
        )?;
        let mut work = crate::session::ShutdownWork::new(
            Some(self.inner.server.clone()),
            self.inner.shutdown_timeout,
        );
        work.route_names = routes.iter().cloned().collect();
        work.relay_anchor_address = self.inner.relay_anchor_address.clone();
        work.relay_use_proxy = self.inner.relay_use_proxy;
        Ok(work)
    }

    /// Read the credential published by this host's bound listener.
    /// The snapshot does not prove the listener is still alive.
    pub fn endpoint_credential(&self) -> Option<c2_local::EndpointCredential> {
        self.inner.server.endpoint_credential()
    }

    /// Exact native endpoint retained for bind, readiness, and credentials.
    pub fn local_endpoint(&self) -> &c2_config::LocalEndpoint {
        self.inner.server.local_endpoint()
    }

    pub fn local_endpoint_context(&self) -> &c2_config::LocalEndpointContext {
        self.local_endpoint().context()
    }

    /// Symmetric read-only memory statistics for this host and its Runtime:
    /// both role domains, retired roles and local holds. Observation never
    /// connects, maps memory, reads the environment, freezes configuration,
    /// or resets accounting.
    pub fn memory_stats(&self) -> crate::RuntimeMemoryStats {
        self.inner.runtime.memory_stats()
    }

    /// Symmetric read-only call-admission snapshot of both role domains.
    pub fn call_execution_snapshot(&self) -> crate::memory::RuntimeCallExecutionStats {
        self.inner.runtime.call_execution_snapshot()
    }

    /// Read-only observer of this host's server-domain accounting.
    ///
    /// The handle shares only the domain counters, so a stopped host keeps
    /// retained response/reassembly charges observable without retaining the
    /// Server, its pools, or its callbacks.
    pub fn server_memory_observer(&self) -> Option<c2_mem::DomainObserver> {
        self.inner.runtime.server_domain_observer()
    }

    pub fn register(&self, definition: ServiceDefinition) -> Result<Registration, Error> {
        self.inner.runtime.ensure_active()?;
        let route_name = definition.expected.route_name.clone();
        let route_spec = route_spec(&definition)?;
        let route_topics = self
            .inner
            .server
            .create_route_topics(definition.expected.clone(), &definition.topics);
        // Commit any staged initial topic frames atomically BEFORE opening the
        // route so early subscribers can read initial state immediately and
        // capacity failures leave the route unopened with zero residual charge.
        for (topic_name, payload) in definition.initial_topics {
            let publisher = route_topics.publisher(&topic_name).map_err(Error::Semantic)?;
            if let Err(error) = publisher.publish(payload) {
                route_topics.close(TopicCloseCause::RouteUnregistered);
                return Err(Error::Semantic(error));
            }
        }
        let route = match &definition.stream_service {
            Some(service) => {
                let callback = Arc::new(CoreStreamCallback {
                    service: service.clone(),
                });
                match definition.stream_descriptor.as_ref() {
                    Some(descriptor) => self
                        .inner
                        .server
                        .build_stream_route(route_spec, callback, descriptor),
                    None => self.inner.server.build_nonportable_stream_route(
                        route_spec,
                        callback,
                        definition
                            .stream_modes
                            .as_deref()
                            .expect("stream method modes"),
                    ),
                }
            }
            None => self.inner.server.build_route(
                route_spec,
                Arc::new(CoreCallback {
                    service: Arc::clone(&definition.service),
                }),
            ),
        };
        let route = match route {
            Ok(route) => route.with_topics(Arc::clone(&route_topics)),
            Err(error) => {
                route_topics.close(TopicCloseCause::RouteUnregistered);
                return Err(LifecycleError::from(error).into());
            }
        };
        let route_concurrency = RouteConcurrency {
            inner: route.route_handle(),
            method_modes: definition
                .stream_modes
                .clone()
                .unwrap_or_else(|| vec![MethodMode::Unary; definition.methods.len()].into()),
            server_budget: self.inner.domain_budget.clone(),
        };
        let outcome = match self.inner.runtime.register_route(
            &self.inner.server,
            route,
            self.inner.relay_anchor_address.as_deref(),
            self.inner.relay_use_proxy,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                route_topics.close(TopicCloseCause::RouteUnregistered);
                return Err(error.into());
            }
        };
        let previous_topics = {
            let mut current_topics = self.inner.route_topics.lock();
            self.inner.route_names.lock().insert(route_name.clone());
            current_topics.insert(route_name.clone(), Arc::clone(&route_topics))
        };
        if let Some(previous) = previous_topics {
            previous.close(TopicCloseCause::RouteUnregistered);
        }
        Ok(Registration {
            host: Arc::clone(&self.inner),
            route_name,
            register_outcome: outcome,
            close_outcome: None,
            route_concurrency,
            route_topics,
        })
    }

    /// Attach a same-process subscription after validating the exact route
    /// name and contract identity. Returns `701 ResourceNotFound` when the
    /// route is absent, `709 ContractMismatch` when the route serves a
    /// different contract, and `713 ProtocolViolation` when the topic is not
    /// declared on the matched contract.
    pub fn subscribe_local(
        &self,
        expected: &ExpectedRouteContract,
        topic_name: &str,
        options: SubscribeOptions,
    ) -> Result<LocalTopicSubscription, Error> {
        validate_expected_route_contract(expected)?;
        let route_topics = self
            .inner
            .route_topics
            .lock()
            .get(&expected.route_name)
            .cloned()
            .ok_or_else(|| {
                Error::Semantic(C2Error::new(
                    ErrorCode::ResourceNotFound,
                    format!("route not found: {}", expected.route_name),
                ))
            })?;
        if route_topics.expected() != expected {
            return Err(Error::Semantic(C2Error::new(
                ErrorCode::ContractMismatch,
                format!(
                    "route {:?} contract identity mismatch",
                    expected.route_name
                ),
            )));
        }
        route_topics
            .subscribe_local(topic_name, options)
            .map(LocalTopicSubscription::from_server)
            .map_err(Error::Semantic)
    }

    pub fn shutdown(&self) -> ShutdownOutcome {
        self.inner.shutdown()
    }

    /// Bound this caller's observation without changing or cancelling the native drain.
    pub fn shutdown_with_timeout(&self, timeout: Duration) -> ShutdownOutcome {
        self.inner.shutdown_with_timeout(timeout)
    }

    pub fn is_running(&self) -> bool {
        self.inner.server.is_running()
    }

    /// Observe the shared terminal native journal without initiating or consuming shutdown.
    pub fn shutdown_outcome(&self) -> Option<ShutdownOutcome> {
        self.inner.refresh_shutdown_outcome();
        self.inner.shutdown_outcome.lock().clone()
    }

    /// Observe the same journal within an existing caller deadline.
    pub fn shutdown_outcome_until(
        &self,
        deadline: Instant,
    ) -> Result<Option<ShutdownOutcome>, LifecycleError> {
        let cached = self
            .inner
            .shutdown_outcome
            .try_lock_until(deadline)
            .ok_or(LifecycleError::ShutdownWaitTimeout {
                stage: "core_host_outcome_wait",
            })?
            .clone();
        if cached.is_some() {
            return Ok(cached);
        }
        self.inner
            .runtime
            .shutdown_observation_until(&self.inner.server, deadline)
    }

    /// Read-only lifecycle observation for this host.
    ///
    /// Reports completion of the original transaction, listener closure and
    /// business drain. Transport storage is observed through memory statistics.
    pub fn lifecycle_snapshot(&self) -> crate::lifecycle::HostLifecycleSnapshot {
        self.inner.lifecycle_snapshot()
    }
}

/// One successful route registration with idempotent explicit and Drop
/// cleanup.
pub struct Registration {
    host: Arc<HostInner>,
    route_name: String,
    register_outcome: RegisterOutcome,
    close_outcome: Option<UnregisterOutcome>,
    route_concurrency: RouteConcurrency,
    route_topics: Arc<RouteTopics>,
}

/// Read-only view of a registration's topic lifecycle, including during drain.
#[derive(Clone)]
pub struct RegistrationObserver {
    route_topics: Arc<RouteTopics>,
}

impl RegistrationObserver {
    pub fn active_subscribers(&self) -> usize {
        self.route_topics.active_subscribers()
    }

    pub fn is_closed(&self) -> bool {
        self.route_topics.is_closed()
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("route_name", &self.route_name)
            .field("register_outcome", &self.register_outcome)
            .field("closed", &self.close_outcome.is_some())
            .finish()
    }
}

impl Registration {
    pub fn route_name(&self) -> &str {
        &self.route_name
    }

    pub fn outcome(&self) -> &RegisterOutcome {
        &self.register_outcome
    }

    pub fn is_closed(&self) -> bool {
        self.close_outcome.is_some() || self.route_topics.is_closed()
    }

    pub fn observer(&self) -> RegistrationObserver {
        RegistrationObserver {
            route_topics: Arc::clone(&self.route_topics),
        }
    }

    pub fn route_concurrency(&self) -> RouteConcurrency {
        self.route_concurrency.clone()
    }

    /// Obtain a registration-scoped publisher handle for a declared topic.
    pub fn publisher(&self, topic_name: &str) -> Result<TopicPublisher, Error> {
        self.route_topics
            .publisher(topic_name)
            .map(TopicPublisher::from_server)
            .map_err(Error::Semantic)
    }

    /// Attach a same-process subscription directly to this registration's topic.
    pub fn subscribe_local(
        &self,
        topic_name: &str,
        options: SubscribeOptions,
    ) -> Result<LocalTopicSubscription, Error> {
        self.route_topics
            .subscribe_local(topic_name, options)
            .map(LocalTopicSubscription::from_server)
            .map_err(Error::Semantic)
    }

    /// Number of live topic subscribers currently attached to this registration.
    pub fn active_subscribers(&self) -> usize {
        self.route_topics.active_subscribers()
    }

    pub fn close(&mut self) -> Result<UnregisterOutcome, Error> {
        if let Some(outcome) = &self.close_outcome {
            return Ok(outcome.clone());
        }
        let outcome = self.host.unregister(&self.route_name, &self.route_topics)?;
        self.close_outcome = Some(outcome.clone());
        Ok(outcome)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Runtime {
    pub fn host(&self, options: HostOptions) -> Result<Host, Error> {
        self.host_with_failed_start_receipt(options, |_| {})
    }

    /// Start one Host and deliver that attempt's native cleanup receipt if
    /// startup fails after the concrete HostInner has been constructed.
    ///
    /// The Runtime's single Host slot merges concurrent same-configuration
    /// starts onto one Host, refuses a different effective configuration as
    /// `ConfigFrozen`, and refuses any restart while the previous close
    /// transaction has not reached its real terminal outcome. A restart after
    /// a complete stop reuses the activated server-domain budget — advancing
    /// its generation and keeping real in-flight accounting — and mints a
    /// fresh `server_instance_id`.
    pub fn host_with_failed_start_receipt(
        &self,
        options: HostOptions,
        on_failed_start: impl FnOnce(FailedStartShutdownReceipt),
    ) -> Result<Host, Error> {
        let (result, receipt) = match self.begin_host_start(&options)? {
            crate::session::HostAdmission::Shared(host) => return Ok(host),
            crate::session::HostAdmission::Starting(attempt) => attempt.wait(),
            crate::session::HostAdmission::Granted(admission, attempt) => {
                let mut receipt = None;
                let result = self.start_admitted_host(
                    admission,
                    #[cfg(test)]
                    &options,
                    &mut receipt,
                );
                self.complete_host_start(&attempt, result.clone(), receipt.clone());
                (result, receipt)
            }
        };
        if let Some(receipt) = receipt {
            on_failed_start(receipt);
        }
        result.map_err(Error::from)
    }

    fn start_admitted_host(
        &self,
        admission: crate::session::HostStart,
        #[cfg(test)] options: &HostOptions,
        failed_receipt: &mut Option<FailedStartShutdownReceipt>,
    ) -> Result<Host, LifecycleError> {
        let startup_timeout = admission.startup_timeout;
        let startup_deadline = c2_config::deadline_after(Instant::now(), startup_timeout)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        let identity = admission.identity;
        let config = admission.ipc;
        let budget = admission.budget.clone();
        let crate::roles::HostSettings {
            relay_anchor_address,
            relay_use_proxy,
            shutdown_timeout,
        } = admission.settings;
        let endpoint = self
            .freeze_host_endpoint_context(
                admission
                    .endpoint_catalog
                    .as_ref()
                    .map(|(revision, catalog)| (*revision, catalog)),
            )?
            .endpoint(&identity.ipc_address)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        let server_identity = ServerIdentity {
            server_id: identity.server_id,
            server_instance_id: identity.server_instance_id,
        };
        // The Runtime's one server-domain budget backs this Host; a restart
        // passes the retained budget so in-flight accounting survives.
        let server = Server::new_with_identity_endpoint_and_budget(
            endpoint,
            config.clone(),
            server_identity,
            budget,
        )
        .map_err(LifecycleError::from)?;
        let lifecycle = crate::lifecycle::HostLifecycleCell::new();
        let server = Arc::new(server.with_connection_event_source(self.connection_event_source()));
        let server_runtime = ServerRuntimeBuilder::build(&config)
            .map_err(|error| LifecycleError::Server(ServerFailure::new(error.to_string())))?;
        server.begin_start_attempt().map_err(LifecycleError::from)?;

        let inner = Arc::new(HostInner {
            runtime: self.clone(),
            server: Arc::clone(&server),
            domain_budget: admission.budget,
            relay_anchor_address,
            relay_use_proxy,
            shutdown_timeout,
            route_names: Mutex::new(HashSet::new()),
            route_topics: Mutex::new(HashMap::new()),
            thread: Mutex::new(None),
            shutdown_outcome: Mutex::new(None),
            shutdown_gate: Mutex::new(()),
            lifecycle: lifecycle.clone(),
        });
        let run_server = Arc::clone(&server);
        #[cfg(test)]
        let startup_gate = options.startup_gate.clone();
        #[cfg(test)]
        let fail_spawn = options
            .startup_gate
            .as_ref()
            .is_some_and(|gate| gate.fail_spawn);
        #[cfg(not(test))]
        let fail_spawn = false;
        let thread = if fail_spawn {
            Err(std::io::Error::other("injected host thread spawn failure"))
        } else {
            std::thread::Builder::new()
                .name(format!("c2-host-{}", server.server_id()))
                .spawn(move || {
                    #[cfg(test)]
                    if let Some(gate) = startup_gate {
                        let _ = gate.entered.send(Arc::clone(&run_server));
                        let _ = gate.proceed.lock().recv();
                    }
                    let result = server_runtime.block_on(
                        run_server.run_with_startup_deadline(startup_deadline, startup_timeout),
                    );
                    if result.is_err() {
                        run_server.finalize_runtime_stopped();
                    }
                    server_runtime.block_on(run_server.wait_connections_settled());
                })
        };
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                // No host thread exists to move Starting to a terminal state.
                // Let the original HostInner Drop run its Core shutdown after
                // the transaction is available for independent observation.
                server.finalize_runtime_stopped();
                *failed_receipt = Some(inner.begin_failed_start_shutdown());
                return Err(LifecycleError::Server(ServerFailure::new(
                    error.to_string(),
                )));
            }
        };
        #[cfg(test)]
        if let Some(join) = options
            .startup_gate
            .as_ref()
            .and_then(|gate| gate.thread_join.as_ref())
        {
            join.send(thread).expect("test owns host thread");
        } else {
            *inner.thread.lock() = Some(thread);
        }
        #[cfg(not(test))]
        {
            *inner.thread.lock() = Some(thread);
        }
        #[cfg(test)]
        let wait_runtime = if options
            .startup_gate
            .as_ref()
            .is_some_and(|gate| gate.fail_wait_runtime)
        {
            Err(LifecycleError::Server(ServerFailure::new(
                "failed to create runtime: injected test failure".to_string(),
            )))
        } else {
            Runtime::server_runtime()
        };
        #[cfg(not(test))]
        let wait_runtime = Runtime::server_runtime();
        let wait_runtime = match wait_runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                // The host thread may already own the accept loop. Signal its
                // existing shutdown path before returning the startup error.
                server.request_shutdown_signal_with_reason("shutdown");
                *failed_receipt = Some(inner.begin_failed_start_shutdown());
                return Err(error);
            }
        };
        if let Err(mut error) = wait_runtime.block_on(
            server
                .wait_until_responsive(startup_deadline.saturating_duration_since(Instant::now())),
        ) {
            if let c2_server::ServerError::StartupTimeout(ref mut expired) = error {
                expired.timeout = startup_timeout;
            }
            if let Some(thread) = inner.thread.lock().take() {
                if thread.is_finished() {
                    let _ = thread.join();
                }
            }
            *failed_receipt = Some(inner.begin_failed_start_shutdown());
            return Err(LifecycleError::from(error));
        }

        Ok(Host { inner })
    }
}

fn route_spec(definition: &ServiceDefinition) -> Result<RouteBuildSpec, Error> {
    let method_names = definition
        .methods
        .iter()
        .map(|method| method.name.to_string())
        .collect::<Vec<_>>();
    let access_map = definition
        .methods
        .iter()
        .map(|method| {
            let access = match method.access {
                MethodAccess::Read => AccessLevel::Read,
                MethodAccess::Write => AccessLevel::Write,
            };
            (method.index, access)
        })
        .collect::<HashMap<_, _>>();
    let route = &definition.expected;
    let limits = SchedulerLimits::try_from_usize(definition.max_pending, definition.max_workers)
        .map_err(|message| method_contract_error("$.concurrency", message))?;
    Ok(RouteBuildSpec {
        name: route.route_name.clone(),
        crm_ns: route.crm_ns.clone(),
        crm_name: route.crm_name.clone(),
        crm_ver: route.crm_ver.clone(),
        abi_hash: route.abi_hash.clone(),
        signature_hash: route.signature_hash.clone(),
        method_names,
        access_map,
        concurrency_mode: definition.concurrency_mode.into(),
        limits,
    })
}

fn callback_error(mut error: C2Error) -> CrmError {
    crate::strip_callback_error_phases(&mut error);
    CrmError::UserError(error.to_wire_bytes())
}

struct CoreCallback {
    service: Arc<dyn EncodedService>,
}

impl CrmCallback for CoreCallback {
    fn invoke(
        &self,
        _route_name: &str,
        method_idx: u16,
        request: Vec<u8>,
        _response_pool: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.service
            .invoke(method_idx, request)
            .map(ResponseMeta::Inline)
            .map_err(callback_error)
    }

    fn invoke_recycled(
        &self,
        _: &str,
        index: u16,
        request: RecycledBytes,
        pool: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.service
            .invoke_into(index, request, &ResponseSink { pool })
            .map(EncodedResponse::into_meta)
            .map_err(callback_error)
    }
}

struct CoreStreamCallback {
    service: Arc<dyn EncodedStreamService>,
}

impl CrmCallback for CoreStreamCallback {
    fn invoke(
        &self,
        _: &str,
        index: u16,
        request: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.service
            .invoke(index, request)
            .map(ResponseMeta::Inline)
            .map_err(callback_error)
    }

    fn invoke_recycled(
        &self,
        _: &str,
        index: u16,
        request: RecycledBytes,
        pool: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.service
            .invoke_into(index, request, &ResponseSink { pool })
            .map(EncodedResponse::into_meta)
            .map_err(callback_error)
    }
}

impl CrmStreamCallback for CoreStreamCallback {
    fn open_stream(
        &self,
        _: &str,
        index: u16,
        request: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        self.service
            .open_stream(index, request)
            .map(|stream| Box::new(CoreStream(stream)) as Box<dyn CrmStream>)
            .map_err(callback_error)
    }

    fn open_stream_recycled(
        &self,
        _: &str,
        index: u16,
        request: RecycledBytes,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        self.service
            .open_stream_recycled(index, request)
            .map(|stream| Box::new(CoreStream(stream)) as Box<dyn CrmStream>)
            .map_err(callback_error)
    }
}

struct CoreStream(Box<dyn EncodedStream>);

impl CrmStream for CoreStream {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        self.0
            .step()
            .map(|item| item.map(|item| Box::new(CoreStreamItem(item)) as Box<dyn CrmStreamItem>))
            .map_err(callback_error)
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        self.0.finish().map_err(callback_error)
    }
}

struct CoreStreamItem(Box<dyn EncodedStreamItem>);

impl CrmStreamItem for CoreStreamItem {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), CrmError> {
        self.0
            .write_range(offset, destination)
            .map_err(callback_error)
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use c2_config::{ConfigField, ServerConfigPatch};
    use tokio::io::AsyncReadExt;

    fn cancel_local_stream_before_first_commit(shutdown: bool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        const WAIT: Duration = Duration::from_secs(10);
        struct NeverInvoked;
        impl EncodedService for NeverInvoked {
            fn invoke(&self, _: u16, _: Vec<u8>) -> Result<Vec<u8>, C2Error> {
                unreachable!("local object calls skip encoded dispatch")
            }
        }
        impl EncodedStreamService for NeverInvoked {
            fn open_stream(&self, _: u16, _: Vec<u8>) -> Result<Box<dyn EncodedStream>, C2Error> {
                unreachable!("local object calls skip encoded dispatch")
            }
        }
        let (runtime, directory) = receipt_test_runtime();
        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        let release = ContractRelease::from_descriptor_json(include_bytes!(
            "../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json"
        ))
        .unwrap();
        let methods = release
            .descriptor()
            .methods()
            .iter()
            .enumerate()
            .map(|(index, method)| MethodDefinition {
                index: index as u16,
                name: method.name().into(),
                access: method.access(),
            });
        let mut registration = host
            .register(
                ServiceDefinition::new_stream(
                    &release,
                    release.reference(),
                    "before-commit",
                    methods,
                    Arc::new(NeverInvoked),
                )
                .unwrap(),
            )
            .unwrap();
        let route = registration.route_concurrency();
        let call = route.inner.begin_local_stream_call(1).unwrap();
        let cancellation = call.cancellation();
        let factories = Arc::new(AtomicUsize::new(0));
        let invoked = factories.clone();
        let (entered, observed) = mpsc::sync_channel(1);
        let (proceed, blocked) = mpsc::channel();
        let stream: CoreLocalResponseStream<u8> = CoreLocalResponseStream::open(
            call,
            move || {
                invoked.fetch_add(1, Ordering::SeqCst);
                unreachable!("cancellation before commit must prevent factory entry")
            },
            host.inner
                .server
                .domain_budget()
                .reserve_operation()
                .unwrap(),
            Some((entered, blocked)),
        )
        .unwrap();
        observed.recv_timeout(WAIT).unwrap();
        assert_eq!(route.snapshot().active_workers, 1);
        let lifecycle = std::thread::spawn(move || {
            if shutdown {
                let outcome = host.shutdown();
                assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
            } else {
                assert!(registration.close().unwrap().local_removed);
            }
        });
        let deadline = Instant::now() + WAIT;
        while cancellation.cause().is_none() {
            assert!(
                Instant::now() < deadline,
                "lifecycle did not submit cancellation"
            );
            std::thread::yield_now();
        }
        assert!(route.snapshot().closed);
        proceed.send(()).unwrap();
        let next = stream
            .next_item()
            .expect_err("lifecycle ends the original stream");
        let receipt = stream.close_with_timeout(Some(WAIT));
        lifecycle.join().unwrap();
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        assert_eq!(route.snapshot().pending, 0);
        drop((stream, route, runtime));
        close_receipt_root(directory);
        assert!(receipt.completed);
        assert_eq!(receipt.complete_items, Some(0));
        let terminal = receipt
            .terminal
            .unwrap()
            .expect_err("original cancellation terminal");
        for error in [next, terminal] {
            let Error::Semantic(error) = error else {
                panic!("expected semantic cancellation")
            };
            assert_eq!(error.code, c2_error::ErrorCode::CallCancelled);
            assert!(
                error
                    .message
                    .contains(if shutdown { "shutdown" } else { "unregister" }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn local_shutdown_before_first_commit_preserves_cancellation_cause() {
        cancel_local_stream_before_first_commit(true);
    }

    #[test]
    fn local_unregister_before_first_commit_preserves_cancellation_cause() {
        cancel_local_stream_before_first_commit(false);
    }

    struct LocalEcho;
    impl EncodedService for LocalEcho {
        fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
            Ok(request)
        }
    }

    fn local_echo_definition() -> ServiceDefinition {
        let release = ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap();
        let methods = release
            .descriptor()
            .methods()
            .iter()
            .enumerate()
            .map(|(index, method)| MethodDefinition {
                index: index as u16,
                name: method.name().into(),
                access: method.access(),
            })
            .collect::<Vec<_>>();
        ServiceDefinition::new(
            &release,
            release.reference(),
            "echo",
            methods,
            Arc::new(LocalEcho),
        )
        .unwrap()
    }

    fn small_shm_host(runtime: &Runtime) -> Host {
        runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_memory_budget_bytes(256 * 1024),
            )
            .unwrap()
    }

    #[test]
    fn core_callback_moves_the_materialized_request_without_a_second_copy() {
        struct RecordsPointer(std::sync::atomic::AtomicUsize);
        impl EncodedService for RecordsPointer {
            fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
                self.0.store(
                    request.as_ptr() as usize,
                    std::sync::atomic::Ordering::Release,
                );
                Ok(request)
            }
        }
        let (runtime, root) = receipt_test_runtime();
        let endpoint = runtime
            .local_endpoint_context()
            .unwrap()
            .endpoint("ipc://callback-move")
            .unwrap();
        let server =
            Server::new_with_endpoint(endpoint, c2_config::ServerIpcConfig::default()).unwrap();
        let service = Arc::new(RecordsPointer(std::sync::atomic::AtomicUsize::new(0)));
        let callback = CoreCallback {
            service: service.clone(),
        };
        let bytes = vec![3; 512];
        let original = bytes.as_ptr() as usize;
        let response = callback
            .invoke_recycled("echo", 1, bytes.into(), server.response_pool_arc())
            .unwrap();
        assert_eq!(
            service.0.load(std::sync::atomic::Ordering::Acquire),
            original
        );
        let ResponseMeta::Inline(bytes) = response else {
            panic!("inline response");
        };
        assert_eq!(bytes.as_ptr() as usize, original);
        drop((server, runtime));
        close_receipt_root(root);
    }

    #[test]
    fn unary_slabs_use_one_client_charge_and_dispatch_into_the_response_sink() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{CallBudget, CallOptions, CallTimeout, Connect};

        const SIZE: usize = 512 * 1024;
        #[derive(Default)]
        struct SlabEcho {
            last_request: AtomicUsize,
        }
        impl EncodedService for SlabEcho {
            fn invoke(&self, _: u16, _: Vec<u8>) -> Result<Vec<u8>, C2Error> {
                panic!("IPC dispatch must call invoke_into")
            }

            fn invoke_into(
                &self,
                _: u16,
                request: RecycledBytes,
                sink: &ResponseSink,
            ) -> Result<EncodedResponse, C2Error> {
                assert_eq!(request.len(), SIZE);
                let ptr = request.as_ptr() as usize;
                let previous = self.last_request.swap(ptr, Ordering::Relaxed);
                if previous != 0 {
                    assert_eq!(ptr, previous, "SHM request copy must reuse the warm buffer");
                }
                assert!(sink.try_alloc_slab(1).unwrap().is_none());
                let mut slab = sink.try_alloc_slab(SIZE).unwrap().expect("response SHM");
                slab.as_mut_slice().copy_from_slice(&request);
                Ok(sink.finish_slab(slab).unwrap())
            }
        }

        let (runtime, root) = receipt_test_runtime();
        runtime
            .update_client_config(crate::ClientConfigPatch {
                domain_limits: crate::DomainLimitsPatch {
                    memory_budget_bytes: crate::ConfigField::Set(SIZE as u64),
                    ..Default::default()
                },
            })
            .unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_memory_budget_bytes(4 * SIZE as u64),
            )
            .unwrap();
        let mut definition = local_echo_definition();
        definition.service = Arc::new(SlabEcho::default());
        let expected = definition.expected_route().clone();
        let registration = host.register(definition).unwrap();
        let client = runtime
            .connect(
                expected,
                Connect::DirectIpc {
                    address: host.local_endpoint().address().into(),
                },
            )
            .unwrap();
        let budget = runtime.client_domain_observer().unwrap();

        let zero = CallBudget::new(
            CallOptions::with_timeout(CallTimeout::After(Duration::ZERO)),
            None,
        )
        .unwrap();
        let Err(Error::Semantic(error)) = client.begin_call_with_budget("echo", zero) else {
            panic!("zero deadline must reject slab preparation");
        };
        assert_eq!(error.code, ErrorCode::CallDeadlineExceeded);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);

        let mut call = client.begin_call("echo").unwrap();
        assert!(call.try_alloc_request_slab(1).unwrap().is_none());
        call.charge_input(SIZE as u64).unwrap();
        let mut slab = call
            .try_alloc_request_slab(SIZE)
            .unwrap()
            .expect("request SHM");
        slab.as_mut_slice().fill(0x42);
        assert_eq!(budget.snapshot().memory.used_bytes, SIZE as u64);
        let held = call.encode_slab(slab).unwrap().call_held().unwrap();
        assert_eq!(held.bytes().len(), SIZE);
        assert!(held.bytes().iter().all(|&byte| byte == 0x42));
        assert_eq!(budget.snapshot().memory.peak_bytes, SIZE as u64);
        drop(held);

        let end = Instant::now() + Duration::from_secs(5);
        while budget.snapshot().memory.used_bytes != 0 {
            assert!(Instant::now() < end, "request loan did not settle");
            std::thread::yield_now();
        }
        let mut call = client.begin_call("echo").unwrap();
        let mut slab = call
            .try_alloc_request_slab(SIZE)
            .unwrap()
            .expect("request SHM");
        slab.as_mut_slice().fill(0x23);
        let owned = call.encode_slab(slab).unwrap().call_owned().unwrap();
        assert_eq!(owned.len(), SIZE);
        assert!(owned.iter().all(|&byte| byte == 0x23));
        assert!(host.shutdown().runtime_barrier_error.is_none());
        drop((registration, host, client, runtime));
        close_receipt_root(root);
    }

    #[test]
    fn local_hold_allows_shm_reuse_and_connection_close_before_release() {
        use crate::{Connect, EncodedClient};
        let (runtime, root) = receipt_test_runtime();
        let host = small_shm_host(&runtime);
        let definition = local_echo_definition();
        let expected = definition.expected_route().clone();
        let registration = host.register(definition).unwrap();
        let response_pool = host.inner.server.response_pool_arc();
        let weak_pool = Arc::downgrade(&response_pool);
        let client = runtime
            .connect(
                expected,
                Connect::DirectIpc {
                    address: host.local_endpoint().address().into(),
                },
            )
            .unwrap();
        let observation = client.connection_observation().unwrap();
        let first = vec![0x5a; 32 * 1024];
        let mut held = client.call_held("echo", &first).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while response_pool.read().stats().alloc_count != 0 {
            assert!(
                Instant::now() < deadline,
                "response loan did not settle while hold lived"
            );
            std::thread::yield_now();
        }
        let before = response_pool.read().stats();
        assert!(before.buddy_expanded_allocs > 0, "test must exercise SHM");
        let second = vec![0x6b; 32 * 1024];
        assert_eq!(client.call_owned("echo", &second).unwrap(), second);
        assert!(response_pool.read().stats().buddy_reused_allocs > before.buddy_reused_allocs);
        assert_eq!(held.bytes(), first);
        let outcome = host.shutdown();
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        let deadline = Instant::now() + Duration::from_secs(5);
        while observation.snapshot().is_none() {
            assert!(
                Instant::now() < deadline,
                "connection close waited for local hold"
            );
            std::thread::yield_now();
        }
        drop((registration, host, response_pool));
        while weak_pool.strong_count() != 0 {
            assert!(
                Instant::now() < deadline,
                "closed Host pool stayed alive with only a local hold"
            );
            std::thread::yield_now();
        }
        assert_eq!(held.bytes(), first);
        held.invalidate_then_release(|| Ok(())).unwrap();
        assert!(held.bytes().is_empty());
        let retirement_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (retirement_outcome, retirement_completed) = runtime
            .begin_session_retirement(
                crate::RuntimeShutdownTarget::ClientsOnly,
                retirement_deadline,
            )
            .expect("begin client-only retirement")
            .observe_until(retirement_deadline);
        assert!(retirement_completed, "{retirement_outcome:?}");
        drop((held, client, runtime));
        close_receipt_root(root);
    }

    #[test]
    fn host_restart_retains_response_loan_in_the_server_domain_until_release() {
        use crate::Connect;
        use c2_ipc::{IpcClient, IpcError};
        use c2_wire::flags::FLAG_HANDSHAKE;
        use c2_wire::frame::{decode_frame, encode_frame};
        use c2_wire::handshake::{
            CAP_SHM_RESPONSE, VersionPreambleKind, decode_handshake, encode_client_handshake,
            encode_version_preamble,
        };
        use c2_wire::release::{ReleaseBatch, encode_release_frame};
        use c2_wire::rpc::{
            self, CallMode, CallStatus, RouteCallIdentity, RpcRecord, ValuePart, ValueStorage,
        };

        const WAIT: Duration = Duration::from_secs(5);
        let (runtime, root) = receipt_test_runtime();
        let options = HostOptions::default()
            .without_relay()
            .with_memory_budget_bytes(64 * 1024);
        let first = runtime.host(options.clone()).unwrap();
        let definition = local_echo_definition();
        let expected = definition.expected_route().clone();
        let registration = first.register(definition).unwrap();

        // Keep a real prepared outgoing request charged to the same Runtime.
        // It is not dispatched, so it cannot account for the server's charge.
        let outgoing = runtime
            .connect(
                expected.clone(),
                Connect::DirectIpc {
                    address: first.local_endpoint().address().into(),
                },
            )
            .unwrap();
        let outgoing_bytes = vec![0x3c; 64];
        let prepared = outgoing
            .begin_call("echo")
            .unwrap()
            .encode_vec(outgoing_bytes)
            .unwrap();
        let client_before = runtime.memory_stats().client;
        let client_operations = runtime.call_execution_snapshot().client;
        assert_eq!(client_before.memory.unwrap().used_bytes, 64);
        assert_eq!(client_operations.used_operations, Some(1));

        let rt = Runtime::server_runtime().unwrap();
        // Unary IpcClient calls copy and release before returning. Keep the
        // actual wire loan on a raw peer so it can outlive Host shutdown.
        let mut peer = rt
            .block_on(c2_local::LocalStream::connect(first.local_endpoint(), WAIT))
            .unwrap();
        let bytes = vec![0x5a; 48 * 1024];
        let (pool_prefix, response) = rt.block_on(async {
            peer.write_all(&encode_version_preamble(VersionPreambleKind::Hello))
                .await
                .unwrap();
            let mut accepted = [0; 25];
            tokio::time::timeout(WAIT, peer.read_exact(&mut accepted))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                accepted,
                encode_version_preamble(VersionPreambleKind::Accepted)
            );
            peer.write_all(&encode_frame(
                0,
                FLAG_HANDSHAKE,
                &encode_client_handshake(CAP_SHM_RESPONSE, "").unwrap(),
            ))
            .await
            .unwrap();
            let ack = read_raw_frame(&mut peer).await;
            let (header, payload) = decode_frame(&ack).unwrap();
            assert!(header.is_handshake());
            let handshake = decode_handshake(payload, true).unwrap();
            assert_ne!(handshake.capability_flags & CAP_SHM_RESPONSE, 0);
            let identity = RouteCallIdentity {
                route_name: expected.route_name.clone(),
                crm_ns: expected.crm_ns.clone(),
                crm_name: expected.crm_name.clone(),
                crm_ver: expected.crm_ver.clone(),
                abi_hash: expected.abi_hash.clone(),
                signature_hash: expected.signature_hash.clone(),
            };
            let call = rpc::encode_open_frame(
                1,
                &identity,
                1,
                CallMode::Unary,
                ValuePart::whole(ValueStorage::Inline(&bytes)),
            )
            .unwrap();
            peer.write_all(&call).await.unwrap();
            (handshake.prefix, read_raw_frame(&mut peer).await)
        });
        let (header, payload) = decode_frame(&response).unwrap();
        let RpcRecord::CallEnd {
            status: CallStatus::Success,
            value:
                ValuePart {
                    storage: ValueStorage::Shm(loan),
                    ..
                },
            ..
        } = rpc::decode_rpc_record(&header, payload, Some(CallMode::Unary)).unwrap()
        else {
            panic!("expected a SHM response loan");
        };
        // A dedicated backing refunds on release while the original Host and
        // pool are still alive.
        assert!(loan.is_dedicated);
        let first_pool = first.inner.server.response_pool_arc();
        let loan_stats = first_pool.read().stats();
        assert_eq!(loan_stats.alloc_count, 1);
        let server_before = runtime.memory_stats().server;
        let memory_before = server_before.memory.unwrap().used_bytes;
        assert_eq!(memory_before, loan_stats.dedicated_active_bytes);
        assert!(memory_before >= bytes.len() as u64);
        assert_ne!(server_before.domain_id, client_before.domain_id);

        let outcome = first.shutdown();
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        assert!(first.lifecycle_snapshot().finished);
        assert!(!first.inner.thread.lock().as_ref().unwrap().is_finished());
        assert_eq!(first_pool.read().stats().alloc_count, 1);
        assert_eq!(runtime.memory_stats().server, server_before);

        let second = runtime.host(options).unwrap();
        assert!(second.is_running());
        assert_eq!(first.local_endpoint(), second.local_endpoint());
        assert_ne!(
            first.inner.server.server_instance_id(),
            second.inner.server.server_instance_id(),
        );
        let restarted = runtime.memory_stats();
        assert_eq!(restarted.server.domain_id, server_before.domain_id);
        assert_eq!(restarted.server.generation, server_before.generation + 1);
        assert_eq!(restarted.server.memory.unwrap().used_bytes, memory_before);
        assert_eq!(restarted.client, client_before);
        assert_eq!(runtime.call_execution_snapshot().client, client_operations);

        let second_registration = second.register(local_echo_definition()).unwrap();
        let mut new_peer = IpcClient::with_endpoint(second.local_endpoint().clone()).unwrap();
        let probe = vec![0x3c; 24 * 1024];
        let binding = rt.block_on(async {
            tokio::time::timeout(WAIT, async {
                new_peer.connect().await.unwrap();
                let binding = new_peer.acquire_route(&expected).await.unwrap();
                let error = new_peer
                    .call_bound(&binding, "echo", &probe)
                    .await
                    .unwrap_err();
                let IpcError::CallRejected(error) = error else {
                    panic!("expected an admission refusal from the new Host, got {error}");
                };
                assert_eq!(error.code, c2_error::ErrorCode::ResourceUnavailable);
                assert_eq!(error.details["stage"], "call_input_admission");
                binding
            })
            .await
            .unwrap()
        });

        let mappings =
            c2_mem::PeerMappingCache::new(c2_config::PoolConfig::default(), pool_prefix).unwrap();
        let read = mappings
            .admit(
                loan.seg_idx,
                loan.generation,
                loan.offset,
                loan.data_size,
                loan.is_dedicated,
            )
            .unwrap();
        // SAFETY: this reader is pinned and the original peer has not
        // signalled RELEASE or disconnected.
        assert_eq!(unsafe { read.copy() }.unwrap(), bytes);
        drop(read);
        rt.block_on(async {
            let batch = ReleaseBatch::new(&[loan.loan_id]).unwrap();
            peer.write_all(&encode_release_frame(&batch, false))
                .await
                .unwrap();
            tokio::time::timeout(WAIT, first.inner.server.wait_connections_settled())
                .await
                .unwrap();
        });
        first.inner.thread.lock().take().unwrap().join().unwrap();
        assert_eq!(first_pool.read().stats().alloc_count, 0);
        assert_eq!(runtime.memory_stats().server.memory.unwrap().used_bytes, 0);
        assert_eq!(runtime.memory_stats().client, client_before);
        assert_eq!(first.shutdown_outcome(), Some(outcome));

        let response = rt.block_on(async {
            tokio::time::timeout(WAIT, new_peer.call_bound(&binding, "echo", &probe))
                .await
                .unwrap()
                .unwrap()
        });
        assert_eq!(response.into_owned_bytes().unwrap(), probe);
        drop(prepared);
        assert_eq!(runtime.memory_stats().client.memory.unwrap().used_bytes, 0);
        assert_eq!(
            runtime.call_execution_snapshot().client.used_operations,
            Some(0)
        );
        assert!(second.shutdown().runtime_barrier_error.is_none());
        drop((
            new_peer,
            peer,
            outgoing,
            registration,
            second_registration,
            first,
            second,
            runtime,
            rt,
        ));
        close_receipt_root(root);
    }

    async fn read_raw_frame(peer: &mut c2_local::LocalStream) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut length = [0; 4];
            peer.read_exact(&mut length).await.unwrap();
            let mut frame = vec![0; u32::from_le_bytes(length) as usize + 4];
            frame[..4].copy_from_slice(&length);
            peer.read_exact(&mut frame[4..]).await.unwrap();
            frame
        })
        .await
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn empty_host_projects_bound_listener_credential_until_shutdown() {
        let directory = tempfile::Builder::new()
            .prefix("c2-host-credential-")
            .tempdir_in("/tmp")
            .unwrap();
        let runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        runtime
            .set_local_endpoint(c2_config::LocalEndpointOptions {
                unix_root: Some(directory.path().to_path_buf()),
            })
            .unwrap();
        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        let credential = host
            .endpoint_credential()
            .expect("bound listener credential");
        assert_eq!(credential.endpoint(), host.local_endpoint());
        assert!(host.is_running());
        assert!(host.shutdown().runtime_barrier_error.is_none());
        assert!(host.endpoint_credential().is_none());
    }

    #[test]
    fn unrepresentable_startup_timeout_does_not_freeze_configuration() {
        let runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let result = runtime.host(HostOptions::default().with_startup_timeout(Duration::MAX));
        assert!(matches!(
            result,
            Err(Error::Lifecycle(LifecycleError::Configuration(_)))
        ));
        assert!(runtime.server_id().is_none());
        assert!(!runtime.server_config_frozen());
        runtime
            .update_server_config(ServerConfigPatch {
                server_id: ConfigField::Set("still-configurable".into()),
                startup_timeout: ConfigField::Set(Duration::from_secs(30)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            runtime.server_overrides().startup_timeout,
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn unportable_shutdown_budget_rejects_before_start_or_stop() {
        let (runtime, directory) = receipt_test_runtime();
        let timeout = Duration::from_secs(10_000_000_000_000_000_000);
        let result = runtime.host(HostOptions::default().with_shutdown_timeout(timeout));
        assert!(matches!(
            result,
            Err(Error::Lifecycle(LifecycleError::Configuration(_)))
        ));
        assert!(runtime.server_id().is_none());
        assert!(!runtime.server_config_frozen());

        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        let rejected = host.shutdown_with_timeout(timeout);
        assert!(
            rejected
                .runtime_barrier_error
                .unwrap()
                .contains("portable monotonic clock range")
        );
        assert!(host.is_running());
        assert!(host.shutdown_outcome().is_none());
        assert!(host.shutdown().runtime_barrier_error.is_none());
        drop(host);
        drop(runtime);
        close_receipt_root(directory);
    }

    #[test]
    fn host_startup_deadline_is_configurable_and_not_a_configuration_error() {
        let mut observed_errors = Vec::new();
        for (stored, configured, timeout) in [
            (None, None, Duration::from_secs(5)),
            (
                Some(Duration::from_millis(20)),
                None,
                Duration::from_millis(20),
            ),
            (
                Some(Duration::ZERO),
                Some(Duration::from_secs(5)),
                Duration::from_secs(5),
            ),
        ] {
            let runtime = Runtime::new(crate::RuntimeOptions {
                use_process_relay_anchor: false,
                ..Default::default()
            })
            .unwrap();
            if let Some(timeout) = stored {
                runtime
                    .update_server_config(ServerConfigPatch {
                        startup_timeout: ConfigField::Set(timeout),
                        ..Default::default()
                    })
                    .unwrap();
            }
            let (entered, observed) = std::sync::mpsc::sync_channel(1);
            let (proceed, delayed) = std::sync::mpsc::channel();
            let mut options = HostOptions::default().without_relay();
            if let Some(timeout) = configured {
                options = options.with_startup_timeout(timeout);
            }
            options.startup_gate = Some(Arc::new(StartupGateForTest {
                entered,
                proceed: Mutex::new(delayed),
                thread_join: None,
                fail_spawn: false,
                fail_wait_runtime: false,
            }));
            let started = Instant::now();
            let host_runtime = runtime.clone();
            let worker = std::thread::spawn(move || host_runtime.host(options));
            let server = observed.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(matches!(
                runtime.update_server_config(ServerConfigPatch {
                    startup_timeout: ConfigField::Set(Duration::from_secs(30)),
                    ..Default::default()
                }),
                Err(LifecycleError::ConfigFrozen)
            ));
            // Hold the actual host thread before bind until its caller expires.
            // Dropping `proceed` also releases it if any assertion unwinds.
            let wait_runtime = Runtime::server_runtime().unwrap();
            wait_runtime.block_on(async {
                tokio::time::timeout(
                    timeout + Duration::from_secs(5),
                    server.wait_for_shutdown_requested(),
                )
                .await
                .unwrap();
            });
            let elapsed = started.elapsed();
            proceed.send(()).unwrap();
            let error = match worker.join().unwrap() {
                Ok(host) => {
                    host.shutdown();
                    panic!("delayed startup unexpectedly succeeded");
                }
                Err(error) => error,
            };
            assert!(elapsed >= timeout);
            assert!(!server.is_running());
            eprintln!("configured={timeout:?}, elapsed={elapsed:?}, error={error}");
            assert!(runtime.server_config_frozen());
            assert_eq!(runtime.server_config().unwrap().startup_timeout, timeout);
            observed_errors.push((error, timeout));
        }
        for (error, expected) in observed_errors {
            assert!(
                matches!(error, Error::Lifecycle(LifecycleError::StartupTimeout(crate::ServerStartupTimeout {
                stage: crate::ServerStartupStage::Readiness,
                timeout,
            })) if timeout == expected),
                "{error}"
            );
        }
    }

    struct FailedAttempt {
        server: Arc<Server>,
        proceed: std::sync::mpsc::Sender<()>,
        host_thread: std::thread::JoinHandle<()>,
        receipt: FailedStartShutdownReceipt,
    }

    struct GatedAttempt {
        observed: std::sync::mpsc::Receiver<Arc<Server>>,
        proceed: std::sync::mpsc::Sender<()>,
        host_thread: std::sync::mpsc::Receiver<std::thread::JoinHandle<()>>,
        caller: std::thread::JoinHandle<(Result<Host, Error>, Option<FailedStartShutdownReceipt>)>,
    }

    fn receipt_test_runtime() -> (Runtime, Option<tempfile::TempDir>) {
        let runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        #[cfg(unix)]
        {
            let parent = std::env::var_os("C2_FAILED_START_TEST_ROOT")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let directory = tempfile::Builder::new()
                .prefix("ipc-")
                .tempdir_in(parent)
                .unwrap();
            eprintln!("RECEIPT_CREATED_ROOT {}", directory.path().display());
            runtime
                .set_local_endpoint(c2_config::LocalEndpointOptions {
                    unix_root: Some(directory.path().to_path_buf()),
                })
                .unwrap();
            (runtime, Some(directory))
        }
        #[cfg(not(unix))]
        {
            runtime
                .set_local_endpoint_with_sources(
                    c2_config::LocalEndpointOptions::default(),
                    c2_config::ConfigSources::empty(),
                )
                .unwrap();
            (runtime, None)
        }
    }

    fn failed_start_options() -> HostOptions {
        HostOptions::default()
            .without_relay()
            .with_startup_timeout(Duration::from_millis(30))
            .with_shutdown_timeout(Duration::ZERO)
    }

    fn launch_gated_attempt(runtime: &Runtime, mut options: HostOptions) -> GatedAttempt {
        let (entered, observed) = std::sync::mpsc::sync_channel(1);
        let (proceed, delayed) = std::sync::mpsc::channel();
        let (join_sender, host_thread) = std::sync::mpsc::sync_channel(1);
        options.startup_gate = Some(Arc::new(StartupGateForTest {
            entered,
            proceed: Mutex::new(delayed),
            thread_join: Some(join_sender),
            fail_spawn: false,
            fail_wait_runtime: false,
        }));
        let runtime = runtime.clone();
        let caller = std::thread::spawn(move || {
            let mut receipt = None;
            let result = runtime.host_with_failed_start_receipt(options, |observed| {
                receipt = Some(observed);
            });
            (result, receipt)
        });
        GatedAttempt {
            observed,
            proceed,
            host_thread,
            caller,
        }
    }

    fn launch_failed_attempt(runtime: &Runtime) -> GatedAttempt {
        launch_gated_attempt(runtime, failed_start_options())
    }

    fn failed_attempt(gated: GatedAttempt, expected_timeout: Duration) -> FailedAttempt {
        let server = gated
            .observed
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        let host_thread = gated
            .host_thread
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        let (result, receipt) = gated.caller.join().unwrap();
        assert!(matches!(
            result,
            Err(Error::Lifecycle(LifecycleError::StartupTimeout(
                crate::ServerStartupTimeout {
                    stage: crate::ServerStartupStage::Readiness,
                    timeout,
                }
            ))) if timeout == expected_timeout
        ));
        FailedAttempt {
            server,
            proceed: gated.proceed,
            host_thread,
            receipt: receipt.expect("failed start must deliver its transaction"),
        }
    }

    fn finish_failed_attempt(
        attempt: FailedAttempt,
    ) -> (ShutdownOutcome, FailedStartShutdownReceipt) {
        attempt.proceed.send(()).unwrap();
        attempt.host_thread.join().unwrap();
        let (outcome, completed) = attempt.receipt.observe_with_timeout(Duration::from_secs(5));
        assert!(
            completed,
            "failed Host cleanup did not publish a terminal outcome"
        );
        drop(attempt.server);
        (outcome, attempt.receipt)
    }

    fn close_receipt_root(directory: Option<tempfile::TempDir>) {
        if let Some(directory) = directory {
            let root = directory.path().to_path_buf();
            directory.close().unwrap();
            eprintln!(
                "RECEIPT_REMOVED_ROOT {} exists={}",
                root.display(),
                root.exists()
            );
            assert!(!root.exists());
        }
    }

    #[test]
    fn failed_start_runtime_retirement_observes_original_pending_cleanup() {
        let (runtime, directory) = receipt_test_runtime();
        let attempt = failed_attempt(launch_failed_attempt(&runtime), Duration::from_millis(30));
        let failed_receipt = attempt.receipt.clone();
        let receipt = runtime
            .begin_session_retirement(
                crate::RuntimeShutdownTarget::FailedStart(&failed_receipt),
                Instant::now(),
            )
            .unwrap();
        assert!(!receipt.observe_until(Instant::now()).1);
        assert!(matches!(
            runtime.ensure_active(),
            Err(LifecycleError::RuntimeRetired)
        ));
        let (original, _) = finish_failed_attempt(attempt);
        let (outcome, completed) = receipt.observe_until(Instant::now() + Duration::from_secs(5));
        assert!(completed && outcome.ipc_clients_drained, "{outcome:?}");
        assert_eq!(outcome, original);
        assert_eq!(failed_receipt.terminal_outcome(), Some(outcome));
        drop(runtime);
        close_receipt_root(directory);
    }

    #[test]
    fn failed_host_thread_spawn_delivers_terminal_receipt() {
        let (runtime, directory) = receipt_test_runtime();
        let (entered, _observed) = std::sync::mpsc::sync_channel(1);
        let (_proceed, delayed) = std::sync::mpsc::channel();
        let mut options = HostOptions::default()
            .without_relay()
            .with_shutdown_timeout(Duration::ZERO);
        options.startup_gate = Some(Arc::new(StartupGateForTest {
            entered,
            proceed: Mutex::new(delayed),
            thread_join: None,
            fail_spawn: true,
            fail_wait_runtime: false,
        }));
        let mut receipt = None;
        let error = runtime
            .host_with_failed_start_receipt(options, |observed| receipt = Some(observed))
            .expect_err("injected host thread spawn must fail");
        assert!(
            error
                .to_string()
                .contains("injected host thread spawn failure")
        );
        let receipt = receipt.expect("spawn failure must deliver its transaction");
        let (outcome, completed) = receipt.observe_with_timeout(Duration::from_secs(5));
        assert!(completed, "spawn failure cleanup must reach terminal state");
        assert!(!outcome.server_was_started);
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        drop(receipt);
        drop(runtime);
        close_receipt_root(directory);
    }

    #[test]
    fn failed_wait_runtime_construction_delivers_running_thread_receipt() {
        let (runtime, directory) = receipt_test_runtime();
        let (entered, observed) = std::sync::mpsc::sync_channel(1);
        let (proceed, delayed) = std::sync::mpsc::channel();
        let (join_sender, host_thread) = std::sync::mpsc::sync_channel(1);
        let mut options = HostOptions::default()
            .without_relay()
            .with_shutdown_timeout(Duration::ZERO);
        options.startup_gate = Some(Arc::new(StartupGateForTest {
            entered,
            proceed: Mutex::new(delayed),
            thread_join: Some(join_sender),
            fail_spawn: false,
            fail_wait_runtime: true,
        }));
        let mut receipt = None;
        let error = runtime
            .host_with_failed_start_receipt(options, |observed| receipt = Some(observed))
            .expect_err("injected wait runtime construction must fail");
        assert!(
            error
                .to_string()
                .contains("failed to create runtime: injected test failure")
        );
        let receipt = receipt.expect("wait runtime failure must deliver its transaction");
        let server = observed.recv_timeout(Duration::from_secs(10)).unwrap();
        let host_thread = host_thread.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!receipt.observe_with_timeout(Duration::ZERO).1);
        proceed.send(()).unwrap();
        host_thread.join().unwrap();
        let (outcome, completed) = receipt.observe_with_timeout(Duration::from_secs(5));
        assert!(completed, "host thread cleanup must reach terminal state");
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        drop(server);
        drop(receipt);
        drop(runtime);
        close_receipt_root(directory);
    }

    #[test]
    fn failed_first_start_receipt_tracks_pending_terminal_and_error() {
        let (runtime, directory) = receipt_test_runtime();
        let attempt = failed_attempt(launch_failed_attempt(&runtime), Duration::from_millis(30));
        assert!(runtime.shutdown_pending(&attempt.server));
        let (invalid, completed) = attempt
            .receipt
            .observe_with_timeout(Duration::from_secs(10_000_000_000_000_000_000));
        assert!(!completed);
        assert!(
            invalid
                .runtime_barrier_error
                .unwrap()
                .contains("portable monotonic clock range")
        );
        assert!(attempt.receipt.terminal_outcome().is_none());
        let (pending, completed) = attempt.receipt.observe_with_timeout(Duration::ZERO);
        assert!(!completed);
        assert!(pending.runtime_barrier_error.is_some());
        assert!(attempt.receipt.terminal_outcome().is_none());

        let (terminal, receipt) = finish_failed_attempt(attempt);
        assert_eq!(receipt.terminal_outcome(), Some(terminal.clone()));
        assert_eq!(
            receipt.observe_with_timeout(Duration::ZERO),
            (terminal, true)
        );

        // A terminal cleanup failure remains terminal and retains its field.
        // This test-only injection isolates receipt projection from OS faults.
        receipt.set_terminal_error_for_test("terminal cleanup failure");
        let (failed, completed) = receipt.observe_with_timeout(Duration::ZERO);
        assert!(completed);
        assert_eq!(
            failed.runtime_barrier_error.as_deref(),
            Some("terminal cleanup failure")
        );
        assert_eq!(receipt.terminal_outcome(), Some(failed));
        drop(receipt);
        drop(runtime);
        close_receipt_root(directory);
    }

    #[test]
    fn failed_restart_receipt_does_not_replace_old_host_journal() {
        let (runtime, directory) = receipt_test_runtime();
        let options = HostOptions::default()
            .without_relay()
            .with_shutdown_timeout(Duration::ZERO);
        let old_host = runtime.host(options.clone()).unwrap();
        let old_outcome = old_host.shutdown_with_timeout(Duration::from_secs(5));
        assert!(old_outcome.runtime_barrier_error.is_none());
        let attempt = failed_attempt(
            launch_gated_attempt(&runtime, options),
            Duration::from_secs(5),
        );
        assert!(runtime.shutdown_pending(&attempt.server));
        assert!(!attempt.receipt.observe_with_timeout(Duration::ZERO).1);
        assert_eq!(old_host.shutdown_outcome(), Some(old_outcome.clone()));
        assert_eq!(old_host.shutdown_with_timeout(Duration::ZERO), old_outcome);
        let (_, receipt) = finish_failed_attempt(attempt);
        assert!(receipt.terminal_outcome().is_some());
        assert_eq!(old_host.shutdown_outcome(), Some(old_outcome));
        drop(receipt);
        drop(old_host);
        drop(runtime);
        close_receipt_root(directory);
    }

    fn wait_for_shared_start(runtime: &Runtime) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while runtime.host_start_waiter_count_for_test() == 0 {
            assert!(
                Instant::now() < deadline,
                "second caller did not join Host startup"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn concurrent_same_config_starts_share_one_running_host() {
        let (runtime, directory) = receipt_test_runtime();
        let options = HostOptions::default()
            .without_relay()
            .with_server_id(format!("shared-{}", uuid::Uuid::new_v4().simple()))
            .with_memory_budget_bytes(1 << 20);
        let first = launch_gated_attempt(&runtime, options.clone());
        let server = first.observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let host_thread = first
            .host_thread
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let second = {
            let runtime = runtime.clone();
            let options = options.clone();
            std::thread::spawn(move || runtime.host(options))
        };
        wait_for_shared_start(&runtime);

        let error = runtime
            .host(options.with_memory_budget_bytes(2 << 20))
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Lifecycle(LifecycleError::ConfigFrozen)
        ));
        first.proceed.send(()).unwrap();
        let (first, receipt) = first.caller.join().unwrap();
        assert!(receipt.is_none());
        let first = first.unwrap();
        let second = second.join().unwrap().unwrap();
        assert!(Arc::ptr_eq(&first.inner, &second.inner));
        assert!(Arc::ptr_eq(&first.inner.server, &server));
        let outcome = second.shutdown();
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        host_thread.join().unwrap();
        drop((first, second, server, runtime));
        close_receipt_root(directory);
    }

    #[test]
    fn concurrent_failed_starts_share_the_original_cleanup_receipt() {
        let (runtime, directory) = receipt_test_runtime();
        let timeout = Duration::from_secs(5);
        let options = failed_start_options().with_startup_timeout(timeout);
        let first = launch_gated_attempt(&runtime, options.clone());
        let server = first.observed.recv_timeout(timeout).unwrap();
        let second = {
            let runtime = runtime.clone();
            std::thread::spawn(move || {
                let mut receipt = None;
                let result = runtime.host_with_failed_start_receipt(options, |observed| {
                    receipt = Some(observed);
                });
                (result, receipt)
            })
        };
        wait_for_shared_start(&runtime);
        let host_thread = first.host_thread.recv_timeout(timeout).unwrap();
        let (first_result, first_receipt) = first.caller.join().unwrap();
        let first_error = first_result.unwrap_err();
        let first = FailedAttempt {
            server,
            proceed: first.proceed,
            host_thread,
            receipt: first_receipt.expect("starting caller gets the cleanup receipt"),
        };
        let (result, receipt) = second.join().unwrap();
        assert!(matches!(
            result,
            Err(Error::Lifecycle(LifecycleError::StartupTimeout(crate::ServerStartupTimeout {
                stage: crate::ServerStartupStage::Readiness,
                timeout: observed,
            }))) if observed == timeout
        ));
        let second_receipt = receipt.expect("merged caller gets the original transaction");
        assert_eq!(result.unwrap_err().to_string(), first_error.to_string());
        assert!(first.receipt.same_transaction_for_test(&second_receipt));
        assert!(!second_receipt.observe_with_timeout(Duration::ZERO).1);
        let (outcome, first_receipt) = finish_failed_attempt(first);
        assert_eq!(second_receipt.terminal_outcome(), Some(outcome));
        drop((first_receipt, second_receipt, runtime));
        close_receipt_root(directory);
    }

    #[test]
    fn host_options_freeze_role_capacity_and_reject_later_change() {
        let (runtime, directory) = receipt_test_runtime();
        let server_id = format!("host-capacity-{}", uuid::Uuid::new_v4().simple());
        let options = HostOptions::default()
            .without_relay()
            .with_server_id(&server_id)
            .with_memory_budget_bytes(1 << 20)
            .with_file_spill_budget_bytes(2 << 20)
            .with_max_outstanding_calls(4)
            .with_max_payload_size(1 << 19)
            .with_max_execution_workers(3);
        let first = runtime.host(options.clone()).unwrap();
        assert_eq!(first.inner.server.server_id(), server_id);
        assert_eq!(
            first.local_endpoint().address(),
            format!("ipc://{server_id}")
        );
        let config = runtime.server_config().unwrap();
        assert_eq!(config.ipc.max_payload_size, 1 << 19);
        assert_eq!(config.ipc.max_execution_workers, 3);
        let first_snapshot = runtime.memory_stats().server;
        assert!(first_snapshot.frozen);
        assert_eq!(first_snapshot.memory.unwrap().limit_bytes, 1 << 20);
        assert_eq!(first_snapshot.file_spill.unwrap().limit_bytes, 2 << 20);
        assert_eq!(
            runtime.call_execution_snapshot().server.max_operations,
            Some(4)
        );
        assert!(!runtime.memory_stats().client.frozen);

        runtime
            .update_server_config(ServerConfigPatch::default())
            .unwrap();
        let error = runtime
            .host(options.clone().with_max_outstanding_calls(5))
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Lifecycle(LifecycleError::ConfigFrozen)
        ));
        let outcome = first.shutdown();
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        let second = runtime.host(options).unwrap();
        let second_snapshot = runtime.memory_stats().server;
        assert_eq!(first_snapshot.domain_id, second_snapshot.domain_id);
        assert_eq!(first_snapshot.generation + 1, second_snapshot.generation);
        assert_ne!(
            first.inner.server.server_instance_id(),
            second.inner.server.server_instance_id()
        );
        assert_eq!(
            runtime.call_execution_snapshot().server.max_operations,
            Some(4)
        );
        assert!(second.shutdown().runtime_barrier_error.is_none());
        drop((first, second, runtime));
        close_receipt_root(directory);
    }

    #[test]
    fn real_relay_idle_eviction_does_not_stop_persistent_host() {
        use crate::{Connect, EncodedClient};
        use c2_http::relay::{RelayConfig, RelayServer};
        struct Echo;
        impl EncodedService for Echo {
            fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
                Ok(request)
            }
        }
        let (runtime, directory) = receipt_test_runtime();
        let mut relay = RelayServer::start_with_context_for_test(
            RelayConfig {
                bind: "127.0.0.1:0".into(),
                heartbeat_interval: Duration::ZERO,
                ..Default::default()
            },
            runtime.local_endpoint_context().unwrap(),
            Duration::from_secs(1),
        )
        .unwrap();
        let url = format!("http://{}", relay.local_addr());
        let host = runtime
            .host(
                HostOptions::default()
                    .with_relay_anchor_address(&url)
                    .with_relay_use_proxy(false),
            )
            .unwrap();
        let release = ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap();
        let route = format!("relay-idle-{}", uuid::Uuid::new_v4().simple());
        let methods = release
            .descriptor()
            .methods()
            .iter()
            .enumerate()
            .map(|(index, method)| MethodDefinition {
                index: index as u16,
                name: method.name().to_string(),
                access: method.access(),
            })
            .collect::<Vec<_>>();
        let _registration = host
            .register(
                ServiceDefinition::new(
                    &release,
                    release.reference(),
                    &route,
                    methods,
                    Arc::new(Echo),
                )
                .unwrap(),
            )
            .unwrap();
        let client_runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        client_runtime
            .inherit_local_endpoint_selection(&runtime)
            .unwrap();
        let client = client_runtime
            .connect(
                release.expected_route(&route).unwrap(),
                Connect::ExplicitRelay {
                    relay_url: url.clone(),
                },
            )
            .unwrap();
        assert_eq!(
            client.call_owned("echo", b"before-idle").unwrap(),
            b"before-idle"
        );
        let business_and_control_connections = host.inner.server.active_connection_count();
        assert!(business_and_control_connections > 0);
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(8);
        while host.inner.server.active_connection_count() >= business_and_control_connections {
            assert!(
                Instant::now() < deadline,
                "real relay upstream must be idle-evicted"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(host.is_running());
        assert!(!host.lifecycle_snapshot().finished);
        let client = client_runtime
            .connect(
                release.expected_route(&route).unwrap(),
                Connect::ExplicitRelay { relay_url: url },
            )
            .unwrap();
        assert_eq!(
            client.call_owned("echo", b"after-idle").unwrap(),
            b"after-idle"
        );
        host.shutdown();
        relay.stop().unwrap();
        drop(client);
        drop(_registration);
        drop(host);
        drop(client_runtime);
        drop(runtime);
        close_receipt_root(directory);
    }
    #[test]
    fn stopped_listener_with_pending_relay_withdraw_blocks_restart_and_identity_reset() {
        use std::io::{Read, Write};
        struct Echo;
        impl EncodedService for Echo {
            fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
                Ok(request)
            }
        }
        let runtime = Runtime::new(crate::RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let options = HostOptions::default()
            .without_relay()
            .with_shutdown_timeout(Duration::from_millis(100));
        let host = runtime.host(options.clone()).unwrap();
        let release = ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap();
        let route = format!("withdraw-fence-{}", uuid::Uuid::new_v4().simple());
        let methods = release
            .descriptor()
            .methods()
            .iter()
            .enumerate()
            .map(|(index, method)| MethodDefinition {
                index: index as u16,
                name: method.name().to_string(),
                access: method.access(),
            })
            .collect::<Vec<_>>();
        let registration = host
            .register(
                ServiceDefinition::new(
                    &release,
                    release.reference(),
                    &route,
                    methods.clone(),
                    Arc::new(Echo),
                )
                .unwrap(),
            )
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            // Accepted sockets inherit O_NONBLOCK on macOS. This peer uses
            // blocking reads with a bounded timeout on every platform.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("/_unregister"));
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
        });
        let incomplete = runtime.shutdown(
            Some(&host.inner.server),
            vec![route.clone()],
            Some(&relay_url),
            false,
            None,
            Duration::from_millis(100),
        );
        assert!(incomplete.runtime_barrier_error.is_some());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            !host.is_running(),
            "listener must already be terminal while relay teardown waits"
        );
        let restart = runtime.host(options.clone()).unwrap_err();
        assert!(restart.to_string().contains("pending native teardown"));
        assert!(runtime.clear_server_identity().is_err());
        assert!(
            runtime
                .update_server_config(ServerConfigPatch {
                    server_id: ConfigField::Set("forbidden-new-identity".into()),
                    ..Default::default()
                })
                .is_err()
        );
        release_tx.send(()).unwrap();
        peer.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while host.shutdown_outcome().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(host.shutdown().relay_errors.is_empty());
        drop(registration);
        drop(host);
        let next = runtime.host(options).unwrap();
        let _next_registration = next
            .register(
                ServiceDefinition::new(
                    &release,
                    release.reference(),
                    &route,
                    methods,
                    Arc::new(Echo),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(next.is_running());
        assert!(next.shutdown_outcome().is_none());
        assert_eq!(next.shutdown().removed_routes, vec![route]);
    }
}
