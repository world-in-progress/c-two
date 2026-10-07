use std::collections::BTreeMap;

use c2_wire::route_catalog_control::{
    RouteContractWire, RouteListResponse, RouteLookupRequest, RouteLookupResponse, RouteMethodWire,
    RouteRecordWire, RouteSelector, RouteStateReasonWire, RouteStateWire, RouteWatchEvent,
};

use crate::dispatcher::CrmRoute;
use crate::server::ServerIdentity;

const DEFAULT_EVENT_RETENTION: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteWatchBatch {
    Events {
        current_revision: u64,
        events: Vec<RouteWatchEvent>,
    },
    Compacted {
        compacted_revision: u64,
        current_revision: u64,
    },
}

#[derive(Debug, Clone)]
struct RemovedRouteTombstone {
    route_uid: String,
    _contract: RouteContractWire,
    _catalog_revision: u64,
    _reason: RouteStateReasonWire,
}

#[derive(Debug, Clone)]
struct StoredWatchEvent {
    route_name: String,
    contract: RouteContractWire,
    event: RouteWatchEvent,
}

#[derive(Debug)]
pub(crate) struct RouteCatalog {
    owner: ServerIdentity,
    owner_epoch: u64,
    max_payload_size: u64,
    catalog_revision: u64,
    min_watch_revision: u64,
    event_retention: usize,
    routes: BTreeMap<String, RouteRecordWire>,
    removed: BTreeMap<String, RemovedRouteTombstone>,
    events: Vec<StoredWatchEvent>,
}

impl RouteCatalog {
    pub(crate) fn new(owner: ServerIdentity, max_payload_size: u64) -> Self {
        Self::with_event_retention(owner, max_payload_size, DEFAULT_EVENT_RETENTION)
    }

    pub(crate) fn with_event_retention(
        owner: ServerIdentity,
        max_payload_size: u64,
        event_retention: usize,
    ) -> Self {
        Self {
            owner,
            owner_epoch: 1,
            max_payload_size,
            catalog_revision: 0,
            min_watch_revision: 1,
            event_retention,
            routes: BTreeMap::new(),
            removed: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    pub(crate) fn list(&self, selector: &RouteSelector) -> Result<RouteListResponse, String> {
        validate_selector(selector)?;
        let routes = self
            .routes
            .values()
            .filter(|record| selector_matches_record(selector, record))
            .cloned()
            .collect();
        Ok(RouteListResponse {
            catalog_revision: self.catalog_revision,
            min_watch_revision: self.min_watch_revision,
            routes,
        })
    }

    pub(crate) fn registered_identity(&self, name: &str) -> Option<(String, u64)> {
        self.routes
            .get(name)
            .map(|route| (route.route_uid.clone(), route.route_revision))
    }

    pub(crate) fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }

    pub(crate) fn contains_current_route(&self, route_name: &str) -> bool {
        self.routes.contains_key(route_name)
    }

    pub(crate) fn lookup(
        &self,
        request: &RouteLookupRequest,
    ) -> Result<RouteLookupResponse, String> {
        validate_expected_contract(&request.expected)?;
        validate_observed_token(request)?;
        let route_name = request.expected.route_name.as_str();
        let Some(record) = self.routes.get(route_name) else {
            if let Some(tombstone) = self.removed.get(route_name) {
                return Ok(RouteLookupResponse::Removed {
                    route_name: route_name.to_string(),
                    route_uid: Some(tombstone.route_uid.clone()),
                });
            }
            return Ok(RouteLookupResponse::NotFound {
                route_name: route_name.to_string(),
            });
        };
        if record.contract != request.expected {
            return Ok(RouteLookupResponse::ContractMismatch {
                current: record.clone(),
            });
        }
        if observed_token_is_stale(request, record) {
            return Ok(RouteLookupResponse::Stale {
                current: record.clone(),
            });
        }
        if !matches!(record.state, RouteStateWire::Ready) {
            return Ok(RouteLookupResponse::Closed {
                route_name: record.route_name.clone(),
                route_uid: record.route_uid.clone(),
                reason: record
                    .state_reason
                    .clone()
                    .unwrap_or(RouteStateReasonWire::ProtocolViolation),
            });
        }
        Ok(RouteLookupResponse::Ready {
            current: record.clone(),
        })
    }

    pub(crate) fn register_ready(
        &mut self,
        route: &CrmRoute,
        admission_open: bool,
    ) -> Result<(), String> {
        if self.routes.contains_key(&route.name) {
            return Err(format!("route already registered: {}", route.name));
        }
        self.removed.remove(&route.name);
        self.catalog_revision = self.next_revision()?;
        let state = if admission_open {
            RouteStateWire::Ready
        } else {
            RouteStateWire::Closed
        };
        let record = self.record_from_route(
            route,
            self.catalog_revision,
            state,
            Some(RouteStateReasonWire::RegisterCommitted),
        );
        self.routes.insert(route.name.clone(), record.clone());
        self.push_event(StoredWatchEvent {
            route_name: record.route_name.clone(),
            contract: record.contract.clone(),
            event: RouteWatchEvent::Added { record },
        });
        Ok(())
    }

    pub(crate) fn open_route(&mut self, route_name: &str) -> Result<(), String> {
        self.update_route_state(
            route_name,
            RouteStateWire::Ready,
            Some(RouteStateReasonWire::RegisterCommitted),
            true,
        )
    }

    pub(crate) fn close_route(
        &mut self,
        route_name: &str,
        reason: RouteStateReasonWire,
    ) -> Result<(), String> {
        self.update_route_state(route_name, RouteStateWire::Draining, Some(reason), false)
    }

    pub(crate) fn remove_route(
        &mut self,
        route_name: &str,
        reason: RouteStateReasonWire,
    ) -> Result<(), String> {
        let Some(record) = self.routes.remove(route_name) else {
            return Err(format!("route not registered: {route_name}"));
        };
        self.catalog_revision = self.next_revision()?;
        self.removed.insert(
            route_name.to_string(),
            RemovedRouteTombstone {
                route_uid: record.route_uid.clone(),
                _contract: record.contract.clone(),
                _catalog_revision: self.catalog_revision,
                _reason: reason.clone(),
            },
        );
        self.push_event(StoredWatchEvent {
            route_name: route_name.to_string(),
            contract: record.contract,
            event: RouteWatchEvent::Removed {
                route_name: route_name.to_string(),
                route_uid: record.route_uid,
                catalog_revision: self.catalog_revision,
                reason,
            },
        });
        Ok(())
    }

    pub(crate) fn watch_from(
        &self,
        from_revision: u64,
        selector: &RouteSelector,
    ) -> RouteWatchBatch {
        if from_revision.saturating_add(1) < self.min_watch_revision {
            return RouteWatchBatch::Compacted {
                compacted_revision: self.min_watch_revision.saturating_sub(1),
                current_revision: self.catalog_revision,
            };
        }
        let events = self
            .events
            .iter()
            .filter(|stored| event_revision(&stored.event) > from_revision)
            .filter(|stored| selector_matches_event(selector, stored))
            .map(|stored| stored.event.clone())
            .collect();
        RouteWatchBatch::Events {
            current_revision: self.catalog_revision,
            events,
        }
    }

    fn update_route_state(
        &mut self,
        route_name: &str,
        state: RouteStateWire,
        reason: Option<RouteStateReasonWire>,
        emit_updated: bool,
    ) -> Result<(), String> {
        if !self.routes.contains_key(route_name) {
            return Err(format!("route not registered: {route_name}"));
        }
        self.catalog_revision = self.next_revision()?;
        let stored_event = {
            let Some(record) = self.routes.get_mut(route_name) else {
                return Err(format!("route not registered: {route_name}"));
            };
            record.state = state;
            record.state_reason = reason.clone();
            record.catalog_revision = self.catalog_revision;
            let event = if emit_updated {
                RouteWatchEvent::Updated {
                    record: record.clone(),
                }
            } else {
                RouteWatchEvent::Closed {
                    route_name: record.route_name.clone(),
                    route_uid: record.route_uid.clone(),
                    catalog_revision: self.catalog_revision,
                    reason: reason.unwrap_or(RouteStateReasonWire::ProtocolViolation),
                }
            };
            StoredWatchEvent {
                route_name: record.route_name.clone(),
                contract: record.contract.clone(),
                event,
            }
        };
        self.push_event(stored_event);
        Ok(())
    }

    fn record_from_route(
        &self,
        route: &CrmRoute,
        catalog_revision: u64,
        state: RouteStateWire,
        state_reason: Option<RouteStateReasonWire>,
    ) -> RouteRecordWire {
        RouteRecordWire {
            route_name: route.name.clone(),
            route_uid: route.route_uid.clone(),
            route_revision: route.route_revision,
            catalog_revision,
            owner_server_id: self.owner.server_id.clone(),
            owner_server_instance_id: self.owner.server_instance_id.clone(),
            owner_epoch: self.owner_epoch,
            contract: RouteContractWire {
                route_name: route.name.clone(),
                crm_ns: route.crm_ns.clone(),
                crm_name: route.crm_name.clone(),
                crm_ver: route.crm_ver.clone(),
                abi_hash: route.abi_hash.clone(),
                signature_hash: route.signature_hash.clone(),
            },
            methods: route
                .method_names
                .iter()
                .enumerate()
                .map(|(index, name)| RouteMethodWire {
                    name: name.clone(),
                    index: index as u16,
                })
                .collect(),
            max_payload_size: self.max_payload_size,
            state,
            state_reason,
            lease_deadline_ms: None,
        }
    }

    fn push_event(&mut self, event: StoredWatchEvent) {
        self.events.push(event);
        while self.events.len() > self.event_retention {
            self.events.remove(0);
        }
        self.min_watch_revision = self
            .events
            .first()
            .map(|stored| event_revision(&stored.event))
            .unwrap_or_else(|| self.catalog_revision.saturating_add(1));
    }

    fn next_revision(&self) -> Result<u64, String> {
        self.catalog_revision
            .checked_add(1)
            .ok_or_else(|| "route catalog revision overflow".to_string())
    }
}

fn validate_selector(selector: &RouteSelector) -> Result<(), String> {
    match selector {
        RouteSelector::All => Ok(()),
        RouteSelector::RouteName { route_name } => validate_route_name(route_name),
        RouteSelector::Contract { expected } => validate_expected_contract(expected),
    }
}

fn validate_expected_contract(contract: &RouteContractWire) -> Result<(), String> {
    let expected = c2_contract::ExpectedRouteContract {
        route_name: contract.route_name.clone(),
        crm_ns: contract.crm_ns.clone(),
        crm_name: contract.crm_name.clone(),
        crm_ver: contract.crm_ver.clone(),
        abi_hash: contract.abi_hash.clone(),
        signature_hash: contract.signature_hash.clone(),
    };
    c2_contract::validate_expected_route_contract(&expected).map_err(|err| err.to_string())
}

fn validate_route_name(route_name: &str) -> Result<(), String> {
    c2_contract::validate_named_route_name("route_name", route_name).map_err(|err| err.to_string())
}

fn validate_observed_token(request: &RouteLookupRequest) -> Result<(), String> {
    match (
        request.observed_route_uid.as_ref(),
        request.observed_route_revision,
    ) {
        (Some(_), Some(_)) | (None, None) => Ok(()),
        _ => Err("observed route token must include both route_uid and route_revision".to_string()),
    }
}

fn observed_token_is_stale(request: &RouteLookupRequest, record: &RouteRecordWire) -> bool {
    match (
        request.observed_route_uid.as_ref(),
        request.observed_route_revision,
    ) {
        (Some(uid), Some(revision)) => {
            uid != &record.route_uid || revision != record.route_revision
        }
        _ => false,
    }
}

fn selector_matches_record(selector: &RouteSelector, record: &RouteRecordWire) -> bool {
    match selector {
        RouteSelector::All => true,
        RouteSelector::RouteName { route_name } => route_name == &record.route_name,
        RouteSelector::Contract { expected } => expected == &record.contract,
    }
}

fn selector_matches_event(selector: &RouteSelector, event: &StoredWatchEvent) -> bool {
    match selector {
        RouteSelector::All => true,
        RouteSelector::RouteName { route_name } => route_name == &event.route_name,
        RouteSelector::Contract { expected } => expected == &event.contract,
    }
}

fn event_revision(event: &RouteWatchEvent) -> u64 {
    match event {
        RouteWatchEvent::Added { record } | RouteWatchEvent::Updated { record } => {
            record.catalog_revision
        }
        RouteWatchEvent::Removed {
            catalog_revision, ..
        }
        | RouteWatchEvent::Closed {
            catalog_revision, ..
        }
        | RouteWatchEvent::Heartbeat {
            catalog_revision, ..
        } => *catalog_revision,
        RouteWatchEvent::Compacted {
            current_revision, ..
        } => *current_revision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Arc;

    use c2_mem::MemPool;
    use c2_wire::route_catalog_control::{
        RouteContractWire, RouteListResponse, RouteLookupRequest, RouteLookupResponse,
        RouteSelector, RouteStateReasonWire, RouteStateWire, RouteWatchEvent,
    };

    use crate::dispatcher::{CrmCallback, CrmError, CrmRoute, RequestData, ResponseMeta};
    use crate::scheduler::{ConcurrencyMode, Scheduler};
    use crate::server::ServerIdentity;

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    struct MockCallback;

    impl CrmCallback for MockCallback {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            _request: RequestData,
            _response_pool: Arc<parking_lot::RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"ok".to_vec()))
        }
    }

    fn owner() -> ServerIdentity {
        ServerIdentity {
            server_id: "simulator-server".into(),
            server_instance_id: "instance-0001".into(),
        }
    }

    fn make_route(name: &str) -> CrmRoute {
        CrmRoute {
            name: name.to_string(),
            route_uid: format!("{name}-uid-0001"),
            route_revision: 1,
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
            scheduler: Arc::new(Scheduler::new(
                ConcurrencyMode::ReadParallel,
                HashMap::new(),
            )),
            callback: Arc::new(MockCallback),
            method_names: vec!["step".into(), "query".into()],
        }
    }

    fn expected_contract(route_name: &str) -> RouteContractWire {
        RouteContractWire {
            route_name: route_name.into(),
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
        }
    }

    fn single_route(
        response: RouteListResponse,
    ) -> c2_wire::route_catalog_control::RouteRecordWire {
        assert_eq!(response.routes.len(), 1);
        response.routes.into_iter().next().unwrap()
    }

    #[test]
    fn list_ready_route_projects_owner_identity_and_catalog_revision() {
        let mut catalog = RouteCatalog::new(owner(), 4096);
        catalog.register_ready(&make_route("grid"), true).unwrap();

        let response = catalog.list(&RouteSelector::All).unwrap();
        let record = single_route(response);

        assert_eq!(record.route_name, "grid");
        assert_eq!(record.route_uid, "grid-uid-0001");
        assert_eq!(record.route_revision, 1);
        assert_eq!(record.catalog_revision, 1);
        assert_eq!(record.owner_server_id, "simulator-server");
        assert_eq!(record.owner_server_instance_id, "instance-0001");
        assert_eq!(record.owner_epoch, 1);
        assert_eq!(record.max_payload_size, 4096);
        assert_eq!(record.state, RouteStateWire::Ready);
        assert_eq!(
            record.state_reason,
            Some(RouteStateReasonWire::RegisterCommitted)
        );
        assert_eq!(record.methods[0].name, "step");
        assert_eq!(record.methods[0].index, 0);
    }

    #[test]
    fn lookup_with_stale_observed_token_returns_current_record() {
        let mut catalog = RouteCatalog::new(owner(), 4096);
        catalog.register_ready(&make_route("grid"), true).unwrap();
        let request = RouteLookupRequest {
            expected: expected_contract("grid"),
            observed_route_uid: Some("old-grid-uid".into()),
            observed_route_revision: Some(1),
        };

        match catalog.lookup(&request).unwrap() {
            RouteLookupResponse::Stale { current } => {
                assert_eq!(current.route_uid, "grid-uid-0001");
                assert_eq!(current.catalog_revision, 1);
            }
            other => panic!("expected stale lookup response, got {other:?}"),
        }
    }

    #[test]
    fn remove_keeps_tombstone_for_lookup_and_watch_events() {
        let mut catalog = RouteCatalog::new(owner(), 4096);
        catalog.register_ready(&make_route("grid"), true).unwrap();
        catalog
            .close_route("grid", RouteStateReasonWire::ExplicitUnregister)
            .unwrap();
        catalog
            .remove_route("grid", RouteStateReasonWire::ExplicitUnregister)
            .unwrap();

        let request = RouteLookupRequest {
            expected: expected_contract("grid"),
            observed_route_uid: Some("grid-uid-0001".into()),
            observed_route_revision: Some(1),
        };
        match catalog.lookup(&request).unwrap() {
            RouteLookupResponse::Removed {
                route_name,
                route_uid,
            } => {
                assert_eq!(route_name, "grid");
                assert_eq!(route_uid.as_deref(), Some("grid-uid-0001"));
            }
            other => panic!("expected removed lookup response, got {other:?}"),
        }

        let RouteWatchBatch::Events { events, .. } = catalog.watch_from(0, &RouteSelector::All)
        else {
            panic!("watch from revision 0 should have retained route events");
        };
        assert!(matches!(events[0], RouteWatchEvent::Added { .. }));
        assert!(matches!(events[1], RouteWatchEvent::Closed { .. }));
        assert!(matches!(events[2], RouteWatchEvent::Removed { .. }));
    }

    #[test]
    fn compacted_watch_reports_history_boundary_without_clearing_current_routes() {
        let mut catalog = RouteCatalog::with_event_retention(owner(), 4096, 2);
        catalog.register_ready(&make_route("grid_a"), true).unwrap();
        catalog.register_ready(&make_route("grid_b"), true).unwrap();
        catalog.register_ready(&make_route("grid_c"), true).unwrap();

        match catalog.watch_from(0, &RouteSelector::All) {
            RouteWatchBatch::Compacted {
                compacted_revision,
                current_revision,
            } => {
                assert_eq!(compacted_revision, 1);
                assert_eq!(current_revision, 3);
            }
            other => panic!("expected compacted watch result, got {other:?}"),
        }

        let response = catalog
            .list(&RouteSelector::RouteName {
                route_name: "grid_c".into(),
            })
            .unwrap();
        let record = single_route(response);
        assert_eq!(record.route_name, "grid_c");
        assert_eq!(record.catalog_revision, 3);
        assert_eq!(record.state, RouteStateWire::Ready);
    }

    #[test]
    fn contract_mismatch_lookup_is_semantic_not_removed() {
        let mut catalog = RouteCatalog::new(owner(), 4096);
        catalog.register_ready(&make_route("grid"), true).unwrap();
        let mut expected = expected_contract("grid");
        expected.abi_hash =
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into();
        let request = RouteLookupRequest {
            expected,
            observed_route_uid: None,
            observed_route_revision: None,
        };

        match catalog.lookup(&request).unwrap() {
            RouteLookupResponse::ContractMismatch { current } => {
                assert_eq!(current.route_name, "grid");
                assert_eq!(current.contract.abi_hash, ABI_HASH);
            }
            other => panic!("expected contract mismatch response, got {other:?}"),
        }
    }

    #[test]
    fn closed_committed_route_becomes_ready_when_admission_opens() {
        let mut catalog = RouteCatalog::new(owner(), 4096);
        catalog.register_ready(&make_route("grid"), false).unwrap();
        let closed = single_route(catalog.list(&RouteSelector::All).unwrap());
        assert_eq!(closed.state, RouteStateWire::Closed);

        catalog.open_route("grid").unwrap();
        let ready = single_route(catalog.list(&RouteSelector::All).unwrap());
        assert_eq!(ready.state, RouteStateWire::Ready);
        assert_eq!(ready.catalog_revision, 2);
    }

    #[test]
    fn failed_state_transition_does_not_advance_catalog_revision() {
        let mut catalog = RouteCatalog::new(owner(), 4096);
        catalog.register_ready(&make_route("grid"), true).unwrap();

        let err = catalog.open_route("missing").unwrap_err();

        assert!(err.contains("route not registered"), "{err}");
        assert_eq!(
            catalog.list(&RouteSelector::All).unwrap().catalog_revision,
            1
        );
    }
}
