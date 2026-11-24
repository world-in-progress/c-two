use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;

use c2_config::CallMetadata;
use c2_error::{C2Error, ErrorCode};

use crate::AccessLevel;

/// Incoming operation presented to the Host admission hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdmissionOperation {
    Unary,
    ServerStream,
    Subscribe,
    Recover,
    QueryOutcome,
}

/// Transport by which the operation reached its Host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdmissionTransport {
    ThreadLocal,
    Ipc,
}

/// Borrowed, read-only facts presented before an operation enters the resource.
pub struct AdmissionContext<'a> {
    route_name: &'a str,
    crm_ns: &'a str,
    crm_name: &'a str,
    crm_ver: &'a str,
    target: Option<&'a str>,
    access: Option<AccessLevel>,
    operation: AdmissionOperation,
    transport: AdmissionTransport,
    call_id: Option<&'a str>,
    metadata: &'a CallMetadata,
}

impl<'a> AdmissionContext<'a> {
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        route_name: &'a str,
        crm_ns: &'a str,
        crm_name: &'a str,
        crm_ver: &'a str,
        target: Option<&'a str>,
        access: Option<AccessLevel>,
        operation: AdmissionOperation,
        transport: AdmissionTransport,
        call_id: Option<&'a str>,
        metadata: &'a CallMetadata,
    ) -> Self {
        Self {
            route_name,
            crm_ns,
            crm_name,
            crm_ver,
            target,
            access,
            operation,
            transport,
            call_id,
            metadata,
        }
    }

    pub fn route_name(&self) -> &'a str {
        self.route_name
    }

    pub fn crm_ns(&self) -> &'a str {
        self.crm_ns
    }

    pub fn crm_name(&self) -> &'a str {
        self.crm_name
    }

    pub fn crm_ver(&self) -> &'a str {
        self.crm_ver
    }

    /// Method or topic name. Outcome queries have no target.
    pub fn target(&self) -> Option<&'a str> {
        self.target
    }

    /// Access declared by the method's contract, when this operation names a method.
    pub fn access(&self) -> Option<AccessLevel> {
        self.access
    }

    pub fn operation(&self) -> AdmissionOperation {
        self.operation
    }

    pub fn transport(&self) -> AdmissionTransport {
        self.transport
    }

    pub fn call_id(&self) -> Option<&'a str> {
        self.call_id
    }

    pub fn metadata(&self) -> &'a CallMetadata {
        self.metadata
    }
}

/// A refusal to enter the resource or control operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRejection {
    pub message: String,
    pub details: BTreeMap<String, String>,
}

impl AdmissionRejection {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: BTreeMap::new(),
        }
    }

    pub fn with_details(mut self, details: BTreeMap<String, String>) -> Self {
        self.details = details;
        self
    }

    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    pub fn into_c2_error(self) -> C2Error {
        let mut details = self.details;
        for key in [
            "transport",
            "transport_phase",
            "dispatch_phase",
            "stage",
            "call_id",
        ] {
            details.remove(key);
        }
        C2Error::new(ErrorCode::AdmissionDenied, self.message).with_details(details)
    }
}

pub enum AdmissionDecision {
    Allow {
        extension: Option<Arc<dyn Any + Send + Sync>>,
    },
    Deny(AdmissionRejection),
}

impl AdmissionDecision {
    pub fn allow() -> Self {
        Self::Allow { extension: None }
    }

    /// Attach Host-owned data for an executing resource method or stream.
    pub fn allow_with_extension(extension: Arc<dyn Any + Send + Sync>) -> Self {
        Self::Allow {
            extension: Some(extension),
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self::Deny(AdmissionRejection::new(message))
    }

    pub fn deny_with_details(
        message: impl Into<String>,
        details: BTreeMap<String, String>,
    ) -> Self {
        Self::Deny(AdmissionRejection::new(message).with_details(details))
    }
}

/// The synchronous Host admission callback. A hook must not synchronously call
/// or query resource operations on the same Host.
pub trait AdmissionHook: Send + Sync + 'static {
    fn admit(&self, context: &AdmissionContext<'_>) -> AdmissionDecision;
}

impl<F> AdmissionHook for F
where
    F: Fn(&AdmissionContext<'_>) -> AdmissionDecision + Send + Sync + 'static,
{
    fn admit(&self, context: &AdmissionContext<'_>) -> AdmissionDecision {
        self(context)
    }
}

#[doc(hidden)]
pub fn evaluate_admission_hook(
    hook: &dyn AdmissionHook,
    context: &AdmissionContext<'_>,
) -> Result<Option<Arc<dyn Any + Send + Sync>>, C2Error> {
    match catch_unwind(AssertUnwindSafe(|| hook.admit(context))) {
        Ok(AdmissionDecision::Allow { extension }) => Ok(extension),
        Ok(AdmissionDecision::Deny(rejection)) => Err(rejection.into_c2_error()),
        Err(payload) => {
            // The panic payload can have a user-defined destructor.
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(payload);
            }
            Err(AdmissionRejection::new("admission hook failed")
                .with_details(BTreeMap::from([("reason".into(), "hook_error".into())]))
                .into_c2_error())
        }
    }
}

/// Read-only context shared by an executing call and its resource callbacks.
#[derive(Clone, Debug)]
pub struct CallContext(Arc<CallContextInner>);

#[derive(Debug)]
struct CallContextInner {
    route_name: String,
    target: String,
    operation: AdmissionOperation,
    call_id: Option<String>,
    metadata: CallMetadata,
    extension: Option<Arc<dyn Any + Send + Sync>>,
}

impl CallContext {
    #[doc(hidden)]
    pub fn new(
        route_name: impl Into<String>,
        target: impl Into<String>,
        operation: AdmissionOperation,
        call_id: Option<String>,
        metadata: CallMetadata,
        extension: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Self {
        Self(Arc::new(CallContextInner {
            route_name: route_name.into(),
            target: target.into(),
            operation,
            call_id,
            metadata,
            extension,
        }))
    }

    pub fn route_name(&self) -> &str {
        &self.0.route_name
    }

    pub fn target(&self) -> &str {
        &self.0.target
    }

    pub fn operation(&self) -> AdmissionOperation {
        self.0.operation
    }

    pub fn call_id(&self) -> Option<&str> {
        self.0.call_id.as_deref()
    }

    pub fn metadata(&self) -> &CallMetadata {
        &self.0.metadata
    }

    pub fn extension<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.extension_raw()?.downcast().ok()
    }

    pub fn extension_raw(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.0.extension.clone()
    }
}

thread_local! {
    static CURRENT_CALL_CONTEXT: RefCell<Option<CallContext>> = const { RefCell::new(None) };
}

/// Returns the call context active on the current resource callback thread.
pub fn current_call_context() -> Option<CallContext> {
    CURRENT_CALL_CONTEXT.with(|current| current.borrow().clone())
}

/// Restores the enclosing callback's context on the thread that entered it.
#[doc(hidden)]
#[must_use]
pub struct CallContextGuard {
    previous: Option<CallContext>,
    _thread_bound: PhantomData<Rc<()>>,
}

#[doc(hidden)]
pub fn enter_call_context(context: CallContext) -> CallContextGuard {
    let previous = CURRENT_CALL_CONTEXT.with(|current| current.replace(Some(context)));
    CallContextGuard {
        previous,
        _thread_bound: PhantomData,
    }
}

impl Drop for CallContextGuard {
    fn drop(&mut self) {
        // Drop the outgoing context after releasing the thread-local borrow.
        CURRENT_CALL_CONTEXT.with(|current| current.replace(self.previous.take()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission_context(metadata: &CallMetadata) -> AdmissionContext<'_> {
        AdmissionContext::new(
            "grid",
            "test.grid",
            "Grid",
            "0.1.0",
            Some("step"),
            Some(AccessLevel::Read),
            AdmissionOperation::Unary,
            AdmissionTransport::Ipc,
            Some("test-call"),
            metadata,
        )
    }

    #[test]
    fn rejection_cannot_override_host_control_details() {
        let hook = |context: &AdmissionContext<'_>| {
            assert_eq!(context.target(), Some("step"));
            assert_eq!(context.access(), Some(AccessLevel::Read));
            assert_eq!(context.metadata().get("tenant"), Some("upstream"));
            AdmissionDecision::deny_with_details(
                "denied",
                BTreeMap::from([
                    ("transport".into(), "http".into()),
                    ("transport_phase".into(), "dispatch_uncertain".into()),
                    ("dispatch_phase".into(), "dispatch_uncertain".into()),
                    ("stage".into(), "execution".into()),
                    ("call_id".into(), "spoofed".into()),
                    ("reason".into(), "policy".into()),
                ]),
            )
        };
        let metadata = CallMetadata::from_entries([("tenant", "upstream")]).unwrap();
        let error = evaluate_admission_hook(&hook, &admission_context(&metadata)).unwrap_err();
        assert_eq!(error.code, ErrorCode::AdmissionDenied);
        assert_eq!(error.message, "denied");
        assert_eq!(
            error.details,
            BTreeMap::from([("reason".into(), "policy".into())])
        );
    }

    #[test]
    fn panicking_hook_fails_closed() {
        let hook = |_: &AdmissionContext<'_>| panic!("private exception message");
        let error =
            evaluate_admission_hook(&hook, &admission_context(&CallMetadata::empty())).unwrap_err();
        assert_eq!(error.code, ErrorCode::AdmissionDenied);
        assert_eq!(error.message, "admission hook failed");
        assert_eq!(error.details["reason"], "hook_error");
    }

    #[test]
    fn nested_context_shadows_and_restores_on_unwind() {
        assert!(current_call_context().is_none());
        {
            let extension = Arc::new(11u32);
            let _outer = enter_call_context(CallContext::new(
                "outer",
                "step",
                AdmissionOperation::Unary,
                Some("test-call".into()),
                CallMetadata::from_entries([("tenant", "upstream")]).unwrap(),
                Some(extension.clone()),
            ));
            assert!(
                std::thread::spawn(|| current_call_context().is_none())
                    .join()
                    .unwrap()
            );
            let nested = catch_unwind(|| {
                let _inner = enter_call_context(CallContext::new(
                    "inner",
                    "step",
                    AdmissionOperation::Unary,
                    None,
                    CallMetadata::empty(),
                    None,
                ));
                let current = current_call_context().unwrap();
                assert_eq!(current.route_name(), "inner");
                assert!(current.metadata().is_empty());
                assert!(current.extension::<u32>().is_none());
                panic!("inner callback failed");
            });
            assert!(nested.is_err());
            let current = current_call_context().unwrap();
            assert_eq!(current.route_name(), "outer");
            assert_eq!(current.call_id(), Some("test-call"));
            assert_eq!(current.metadata().get("tenant"), Some("upstream"));
            assert!(Arc::ptr_eq(
                &current.extension::<u32>().unwrap(),
                &extension
            ));
            assert!(current.extension::<String>().is_none());
        }
        assert!(current_call_context().is_none());
    }
}
