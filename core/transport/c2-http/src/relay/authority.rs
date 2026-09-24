//! Route control-plane authority.
//!
//! All mutating route operations flow through this module so owner checks,
//! peer ownership checks, and route-table updates stay in one state machine.

use std::sync::Arc;

use c2_contract::{ExpectedRouteContract, MAX_WIRE_TEXT_BYTES};
use c2_ipc::IpcClient;

use crate::relay::conn_pool::{
    CachedClient, OwnerReplaceError, OwnerReplacementEvidence, OwnerToken,
};
use crate::relay::route_table::{valid_route_name, validate_server_instance_id_value};
use crate::relay::state::RelayState;
use crate::relay::types::{Locality, RouteEntry, UpstreamEndpointKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlError {
    InvalidName { reason: String },
    InvalidServerId { reason: String },
    InvalidServerInstanceId { reason: String },
    InvalidAddress { reason: String },
    ContractMismatch { reason: String },
    UpstreamUnavailable { reason: String },
    AddressMismatch { existing_address: String },
    DuplicateRoute { existing_address: String },
    OwnerMismatch,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttestedRouteContract {
    pub route_uid: String,
    pub route_revision: u64,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
}

pub(crate) struct ClaimedRouteContract<'a> {
    pub expected: &'a ExpectedRouteContract,
    pub max_payload_size: u64,
}

pub(crate) struct LocalRouteOwner {
    pub name: String,
    pub server_id: String,
    pub server_instance_id: String,
    pub address: String,
}

pub(crate) struct LocalRegistration {
    pub owner: LocalRouteOwner,
    pub contract: AttestedRouteContract,
    pub replacement: Option<OwnerReplacement>,
}

fn existing_contract_mismatch_reason(
    existing: &RouteEntry,
    claimed: &ClaimedRouteContract<'_>,
) -> Option<String> {
    let expected = claimed.expected;
    if existing.crm_ns != expected.crm_ns
        || existing.crm_name != expected.crm_name
        || existing.crm_ver != expected.crm_ver
        || existing.abi_hash != expected.abi_hash
        || existing.signature_hash != expected.signature_hash
        || existing.max_payload_size != claimed.max_payload_size
    {
        return Some(format!(
            "CRM contract mismatch for route '{}': existing {}/{}/{} hashes={}/{} max_payload_size={}, got {}/{}/{} hashes={}/{} max_payload_size={}",
            expected.route_name,
            existing.crm_ns,
            existing.crm_name,
            existing.crm_ver,
            existing.abi_hash,
            existing.signature_hash,
            existing.max_payload_size,
            expected.crm_ns,
            expected.crm_name,
            expected.crm_ver,
            expected.abi_hash,
            expected.signature_hash,
            claimed.max_payload_size
        ));
    }
    None
}

fn control_error_from_ipc_attestation_error(
    route_name: &str,
    err: c2_ipc::IpcError,
) -> ControlError {
    match err {
        c2_ipc::IpcError::RouteNotFound(_)
        | c2_ipc::IpcError::RouteRemoved { .. }
        | c2_ipc::IpcError::RouteClosed { .. } => ControlError::NotFound,
        c2_ipc::IpcError::ContractMismatch(reason) | c2_ipc::IpcError::Protocol(reason) => {
            ControlError::ContractMismatch { reason }
        }
        err => ControlError::UpstreamUnavailable {
            reason: format!("IPC upstream route '{route_name}' acquisition failed: {err}"),
        },
    }
}

pub(crate) async fn read_ipc_route_contract(
    client: &IpcClient,
    route_name: &str,
) -> Result<AttestedRouteContract, ControlError> {
    let Some(contract) = client.route_contract(route_name) else {
        return Err(ControlError::NotFound);
    };
    if contract.crm_ns.is_empty() || contract.crm_name.is_empty() || contract.crm_ver.is_empty() {
        return Err(ControlError::ContractMismatch {
            reason: format!("IPC upstream route '{route_name}' did not advertise a CRM contract"),
        });
    }
    if c2_contract::validate_expected_route_contract(&contract).is_err() {
        return Err(ControlError::ContractMismatch {
            reason: format!("IPC upstream route '{route_name}' advertised an invalid CRM contract"),
        });
    }
    let binding = client
        .attest_route_for_registration(&contract)
        .await
        .map_err(|err| control_error_from_ipc_attestation_error(route_name, err))?;
    Ok(AttestedRouteContract {
        route_uid: binding.route_uid().to_string(),
        route_revision: binding.route_revision(),
        crm_ns: contract.crm_ns,
        crm_name: contract.crm_name,
        crm_ver: contract.crm_ver,
        abi_hash: contract.abi_hash,
        signature_hash: contract.signature_hash,
        max_payload_size: binding.max_payload_size(),
    })
}

pub(crate) async fn attest_ipc_route_contract(
    client: &IpcClient,
    claimed: ClaimedRouteContract<'_>,
) -> Result<AttestedRouteContract, ControlError> {
    let route_name = claimed.expected.route_name.as_str();
    if c2_contract::validate_expected_route_contract(claimed.expected).is_err() {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream route '{route_name}' registration did not claim a complete CRM contract"
            ),
        });
    }
    let binding = client
        .attest_route_for_registration(claimed.expected)
        .await
        .map_err(|err| control_error_from_ipc_attestation_error(route_name, err))?;
    let max_payload_size = binding.max_payload_size();
    if claimed.max_payload_size != max_payload_size {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream route '{route_name}' max_payload_size mismatch: claimed {}, got {max_payload_size}",
                claimed.max_payload_size
            ),
        });
    }
    Ok(AttestedRouteContract {
        route_uid: binding.route_uid().to_string(),
        route_revision: binding.route_revision(),
        crm_ns: claimed.expected.crm_ns.clone(),
        crm_name: claimed.expected.crm_name.clone(),
        crm_ver: claimed.expected.crm_ver.clone(),
        abi_hash: claimed.expected.abi_hash.clone(),
        signature_hash: claimed.expected.signature_hash.clone(),
        max_payload_size,
    })
}

pub(crate) async fn attest_ipc_pending_route_contract(
    client: &mut IpcClient,
    registration_token: &str,
    claimed: ClaimedRouteContract<'_>,
) -> Result<AttestedRouteContract, ControlError> {
    let route_name = claimed.expected.route_name.as_str();
    let (contract, binding) = match client
        .acquire_pending_route_attestation(route_name, registration_token)
        .await
    {
        Ok(attested) => attested,
        Err(c2_ipc::IpcError::RouteNotFound(_)) => return Err(ControlError::NotFound),
        Err(err) => {
            return Err(control_error_from_ipc_attestation_error(route_name, err));
        }
    };
    let max_payload_size = binding.max_payload_size();
    if c2_contract::validate_expected_route_contract(claimed.expected).is_err() {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream route '{route_name}' registration did not claim a complete CRM contract"
            ),
        });
    }
    if claimed.expected != &contract {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream pending route '{route_name}' CRM contract mismatch: claimed {}/{}/{} hashes={}/{}, got {}/{}/{} hashes={}/{}",
                claimed.expected.crm_ns,
                claimed.expected.crm_name,
                claimed.expected.crm_ver,
                claimed.expected.abi_hash,
                claimed.expected.signature_hash,
                contract.crm_ns,
                contract.crm_name,
                contract.crm_ver,
                contract.abi_hash,
                contract.signature_hash,
            ),
        });
    }
    if claimed.max_payload_size != max_payload_size {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream pending route '{route_name}' max_payload_size mismatch: claimed {}, got {max_payload_size}",
                claimed.max_payload_size
            ),
        });
    }
    Ok(AttestedRouteContract {
        route_uid: binding.route_uid().to_string(),
        route_revision: binding.route_revision(),
        crm_ns: contract.crm_ns,
        crm_name: contract.crm_name,
        crm_ver: contract.crm_ver,
        abi_hash: contract.abi_hash,
        signature_hash: contract.signature_hash,
        max_payload_size,
    })
}

#[derive(Clone)]
pub(crate) enum RegisterPreflight {
    Available {
        replacement: Option<Box<OwnerReplacementCandidate>>,
    },
    SameOwner,
}

pub(crate) enum RegisterPreparation {
    Available {
        replacement: Option<Box<OwnerReplacementCandidate>>,
    },
    SameOwner,
    DuplicateAlive {
        existing_address: String,
    },
}

#[derive(Clone)]
pub(crate) struct OwnerReplacement {
    route_name: String,
    server_id: String,
    server_instance_id: String,
    ipc_address: String,
    existing_address: String,
    crm_ns: String,
    crm_name: String,
    crm_ver: String,
    abi_hash: String,
    signature_hash: String,
    max_payload_size: u64,
    route_uid: String,
    route_revision: u64,
    token: OwnerToken,
    evidence: OwnerReplacementEvidence,
}

#[derive(Clone)]
pub(crate) struct OwnerReplacementCandidate {
    route_name: String,
    server_id: String,
    server_instance_id: String,
    ipc_address: String,
    existing_address: String,
    crm_ns: String,
    crm_name: String,
    crm_ver: String,
    abi_hash: String,
    signature_hash: String,
    max_payload_size: u64,
    route_uid: String,
    route_revision: u64,
    token: OwnerToken,
}

impl OwnerReplacementCandidate {
    fn with_evidence(self, evidence: OwnerReplacementEvidence) -> OwnerReplacement {
        OwnerReplacement {
            route_name: self.route_name,
            server_id: self.server_id,
            server_instance_id: self.server_instance_id,
            ipc_address: self.ipc_address,
            existing_address: self.existing_address,
            crm_ns: self.crm_ns,
            crm_name: self.crm_name,
            crm_ver: self.crm_ver,
            abi_hash: self.abi_hash,
            signature_hash: self.signature_hash,
            max_payload_size: self.max_payload_size,
            route_uid: self.route_uid,
            route_revision: self.route_revision,
            token: self.token,
            evidence,
        }
    }
}

pub(crate) enum RouteCommand {
    RegisterLocal(Box<LocalRegistration>),
    UnregisterLocal {
        name: String,
        server_id: String,
    },
    AnnouncePeer {
        sender_relay_id: String,
        entry: Box<RouteEntry>,
    },
    WithdrawPeer {
        sender_relay_id: String,
        name: String,
        relay_id: String,
        removed_at: f64,
        removed_revision: u64,
    },
    RemovePeerRoutes {
        relay_id: String,
    },
}

pub(crate) enum RouteCommandResult {
    Registered {
        entry: RouteEntry,
    },
    SameOwner {
        entry: RouteEntry,
    },
    Unregistered {
        entry: RouteEntry,
        removed_at: f64,
        removed_revision: u64,
        client: Option<Arc<IpcClient>>,
    },
    AlreadyUnregistered,
    PeerRouteChanged,
    PeerRoutesRemoved,
}

pub(crate) struct RouteAuthority<'a> {
    state: &'a RelayState,
}

impl<'a> RouteAuthority<'a> {
    pub(crate) fn new(state: &'a RelayState) -> Self {
        Self { state }
    }

    pub(crate) fn validate_route_name(&self, name: &str) -> Result<(), ControlError> {
        if !valid_route_name(name) {
            return Err(ControlError::InvalidName {
                reason: format!(
                    "name must be non-empty, control-character-free, and no more than {MAX_WIRE_TEXT_BYTES} bytes"
                ),
            });
        }
        Ok(())
    }

    pub(crate) fn validate_relay_id(&self, relay_id: &str) -> Result<(), ControlError> {
        c2_config::validate_relay_id(relay_id).map_err(|_| ControlError::OwnerMismatch)
    }

    pub(crate) fn validate_server_id(&self, server_id: &str) -> Result<(), ControlError> {
        c2_config::validate_server_id(server_id)
            .map_err(|reason| ControlError::InvalidServerId { reason })?;
        if server_id.len() > MAX_WIRE_TEXT_BYTES {
            return Err(ControlError::InvalidServerId {
                reason: format!("server_id cannot exceed {MAX_WIRE_TEXT_BYTES} bytes"),
            });
        }
        Ok(())
    }

    pub(crate) fn validate_server_instance_id(
        &self,
        server_instance_id: &str,
    ) -> Result<(), ControlError> {
        validate_server_instance_id_value(server_instance_id)
            .map_err(|reason| ControlError::InvalidServerInstanceId { reason })
    }

    pub(crate) fn validate_ipc_address(&self, address: &str) -> Result<(), ControlError> {
        c2_ipc::local_endpoint_from_ipc_address(address)
            .map(|_| ())
            .map_err(|err| ControlError::InvalidAddress {
                reason: err.to_string(),
            })
    }

    pub(crate) fn validate_crm_tag(
        &self,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
    ) -> Result<(), ControlError> {
        c2_contract::validate_crm_tag(crm_ns, crm_name, crm_ver).map_err(|err| {
            ControlError::ContractMismatch {
                reason: err.to_string(),
            }
        })
    }

    pub(crate) fn register_local_preflight(
        &self,
        name: &str,
        server_id: &str,
        server_instance_id: &str,
        address: &str,
    ) -> Result<RegisterPreflight, ControlError> {
        self.validate_route_name(name)?;
        self.validate_server_id(server_id)?;
        self.validate_server_instance_id(server_instance_id)?;
        self.validate_ipc_address(address)?;

        let existing = self.state.local_route(name);
        let Some(existing) = existing else {
            return Ok(RegisterPreflight::Available { replacement: None });
        };

        let existing_address = existing.ipc_address.clone().unwrap_or_default();
        let existing_server_id = existing.server_id.as_deref().unwrap_or_default();
        let existing_server_instance_id =
            existing.server_instance_id.as_deref().unwrap_or_default();
        if existing_server_id == server_id {
            if existing_address == address {
                if existing_server_instance_id == server_instance_id {
                    return Ok(RegisterPreflight::SameOwner);
                }
                return Ok(RegisterPreflight::Available { replacement: None });
            }
            return Err(ControlError::AddressMismatch { existing_address });
        }

        let replacement = self.different_owner_replacement(&existing)?;
        Ok(RegisterPreflight::Available {
            replacement: Some(Box::new(replacement)),
        })
    }

    pub(crate) async fn prepare_candidate_registration(
        &self,
        name: &str,
        server_id: &str,
        address: &str,
    ) -> Result<Option<Box<OwnerReplacementCandidate>>, ControlError> {
        let mut last_stale_owner = None;
        for _ in 0..3 {
            self.validate_route_name(name)?;
            self.validate_server_id(server_id)?;
            self.validate_ipc_address(address)?;

            let existing = self.state.local_route(name);
            let Some(existing) = existing else {
                return Ok(None);
            };

            let existing_address = existing.ipc_address.clone().unwrap_or_default();
            let existing_server_id = existing.server_id.as_deref().unwrap_or_default();
            if existing_server_id == server_id {
                if existing_address == address {
                    return Ok(None);
                }
                return Err(ControlError::AddressMismatch { existing_address });
            }

            let replacement = match self.different_owner_replacement(&existing) {
                Ok(replacement) => replacement,
                Err(ControlError::DuplicateRoute { existing_address }) => {
                    return Err(ControlError::DuplicateRoute { existing_address });
                }
                Err(err) => return Err(err),
            };

            match self.probe_captured_owner(&replacement).await {
                OwnerProbe::Alive => {
                    return Err(ControlError::DuplicateRoute {
                        existing_address: replacement.existing_address,
                    });
                }
                OwnerProbe::RouteMissing | OwnerProbe::Dead => {
                    return Ok(Some(Box::new(replacement)));
                }
                OwnerProbe::Stale => {
                    last_stale_owner = Some(replacement.existing_address);
                }
            }
        }

        Err(ControlError::DuplicateRoute {
            existing_address: last_stale_owner.unwrap_or_else(|| "<unknown>".to_string()),
        })
    }

    pub(crate) async fn prepare_register(
        &self,
        name: &str,
        server_id: &str,
        server_instance_id: &str,
        address: &str,
    ) -> Result<RegisterPreparation, ControlError> {
        let mut last_stale_owner = None;
        for _ in 0..3 {
            match self.register_local_preflight(name, server_id, server_instance_id, address)? {
                RegisterPreflight::Available { replacement: None } => {
                    return Ok(RegisterPreparation::Available { replacement: None });
                }
                RegisterPreflight::SameOwner => return Ok(RegisterPreparation::SameOwner),
                RegisterPreflight::Available {
                    replacement: Some(replacement),
                } => match self.probe_captured_owner(&replacement).await {
                    OwnerProbe::Alive => {
                        return Ok(RegisterPreparation::DuplicateAlive {
                            existing_address: replacement.existing_address,
                        });
                    }
                    OwnerProbe::RouteMissing | OwnerProbe::Dead => {
                        return Ok(RegisterPreparation::Available {
                            replacement: Some(replacement),
                        });
                    }
                    OwnerProbe::Stale => {
                        last_stale_owner = Some(replacement.existing_address);
                    }
                },
            }
        }
        Ok(RegisterPreparation::DuplicateAlive {
            existing_address: last_stale_owner.unwrap_or_else(|| "<unknown>".to_string()),
        })
    }

    pub(crate) async fn confirm_replacement_for_commit(
        &self,
        replacement: Option<Box<OwnerReplacementCandidate>>,
    ) -> Result<Option<OwnerReplacement>, ControlError> {
        let Some(replacement) = replacement else {
            return Ok(None);
        };

        match self.probe_captured_owner(&replacement).await {
            OwnerProbe::Alive => Err(ControlError::DuplicateRoute {
                existing_address: replacement.existing_address,
            }),
            OwnerProbe::RouteMissing => Ok(Some(
                (*replacement).with_evidence(OwnerReplacementEvidence::ConfirmedRouteMissing),
            )),
            OwnerProbe::Dead => Ok(Some(
                (*replacement).with_evidence(OwnerReplacementEvidence::ConfirmedDead),
            )),
            OwnerProbe::Stale => Err(ControlError::DuplicateRoute {
                existing_address: replacement.existing_address,
            }),
        }
    }

    pub(crate) fn execute(
        &self,
        command: RouteCommand,
    ) -> Result<RouteCommandResult, ControlError> {
        match command {
            RouteCommand::RegisterLocal(registration) => self.register_local(*registration),
            RouteCommand::UnregisterLocal { name, server_id } => {
                self.unregister_local(name, server_id)
            }
            RouteCommand::AnnouncePeer {
                sender_relay_id,
                entry,
            } => self.announce_peer(sender_relay_id, *entry),
            RouteCommand::WithdrawPeer {
                sender_relay_id,
                name,
                relay_id,
                removed_at,
                removed_revision,
            } => self.withdraw_peer(
                sender_relay_id,
                name,
                relay_id,
                removed_at,
                removed_revision,
            ),
            RouteCommand::RemovePeerRoutes { relay_id } => {
                self.remove_peer_routes(&relay_id);
                Ok(RouteCommandResult::PeerRoutesRemoved)
            }
        }
    }

    fn register_local(
        &self,
        registration: LocalRegistration,
    ) -> Result<RouteCommandResult, ControlError> {
        let LocalRegistration {
            owner:
                LocalRouteOwner {
                    name,
                    server_id,
                    server_instance_id,
                    address,
                },
            contract:
                AttestedRouteContract {
                    route_uid,
                    route_revision,
                    crm_ns,
                    crm_name,
                    crm_ver,
                    abi_hash,
                    signature_hash,
                    max_payload_size,
                },
            replacement,
        } = registration;
        self.validate_route_name(&name)?;
        self.validate_server_id(&server_id)?;
        self.validate_server_instance_id(&server_instance_id)?;
        self.validate_ipc_address(&address)?;
        self.validate_crm_tag(&crm_ns, &crm_name, &crm_ver)?;
        c2_contract::validate_contract_hash("abi_hash", &abi_hash).map_err(|err| {
            ControlError::ContractMismatch {
                reason: err.to_string(),
            }
        })?;
        c2_contract::validate_contract_hash("signature_hash", &signature_hash).map_err(|err| {
            ControlError::ContractMismatch {
                reason: err.to_string(),
            }
        })?;
        if max_payload_size == 0 {
            return Err(ControlError::ContractMismatch {
                reason: "max_payload_size must be > 0".to_string(),
            });
        }
        c2_contract::validate_call_route_key("route_uid", &route_uid).map_err(|err| {
            ControlError::ContractMismatch {
                reason: err.to_string(),
            }
        })?;
        if route_revision == 0 {
            return Err(ControlError::ContractMismatch {
                reason: "route_revision must be > 0".to_string(),
            });
        }

        let mut route_table = self.state.route_table_write();
        let mut required_replacement_address = None;
        let mut old_endpoint_for_cleanup = None;
        if let Some(existing) = route_table.local_route(&name) {
            let existing_address = existing.ipc_address.clone().unwrap_or_default();
            let existing_server_id = existing.server_id.clone().unwrap_or_default();
            let existing_server_instance_id =
                existing.server_instance_id.clone().unwrap_or_default();
            let existing_endpoint =
                UpstreamEndpointKey::from_route(&existing).ok_or(ControlError::OwnerMismatch)?;
            if existing_server_id == server_id {
                if existing_address == address {
                    if existing_server_instance_id == server_instance_id {
                        let expected = ExpectedRouteContract {
                            route_name: name.clone(),
                            crm_ns: crm_ns.clone(),
                            crm_name: crm_name.clone(),
                            crm_ver: crm_ver.clone(),
                            abi_hash: abi_hash.clone(),
                            signature_hash: signature_hash.clone(),
                        };
                        if let Some(reason) = existing_contract_mismatch_reason(
                            &existing,
                            &ClaimedRouteContract {
                                expected: &expected,
                                max_payload_size,
                            },
                        ) {
                            return Err(ControlError::ContractMismatch { reason });
                        }
                        if existing.route_uid == route_uid
                            && existing.route_revision == route_revision
                        {
                            if let Some(token) =
                                self.state.owner_token_for_endpoint(&existing_endpoint)
                            {
                                self.state
                                    .renew_owner_lease_for_endpoint(&existing_endpoint, &token);
                            }
                            return Ok(RouteCommandResult::SameOwner { entry: existing });
                        }
                    } else {
                        old_endpoint_for_cleanup = Some(existing_endpoint.clone());
                    }
                } else {
                    return Err(ControlError::AddressMismatch { existing_address });
                }
            } else {
                match replacement.as_ref() {
                    Some(token) if token.existing_address == existing_address => {
                        if token.route_name != name
                            || token.server_id != existing_server_id
                            || token.server_instance_id != existing_server_instance_id
                            || token.ipc_address != existing_address
                            || token.crm_ns != existing.crm_ns
                            || token.crm_name != existing.crm_name
                            || token.crm_ver != existing.crm_ver
                            || token.abi_hash != existing.abi_hash
                            || token.signature_hash != existing.signature_hash
                            || token.max_payload_size != existing.max_payload_size
                            || token.route_uid != existing.route_uid
                            || token.route_revision != existing.route_revision
                        {
                            return Err(ControlError::DuplicateRoute { existing_address });
                        }
                    }
                    _ => return Err(ControlError::DuplicateRoute { existing_address }),
                }
                required_replacement_address = Some(existing_address);
                old_endpoint_for_cleanup = Some(existing_endpoint);
            }
        }

        let entry = RouteEntry {
            name: name.clone(),
            relay_id: self.state.relay_id().to_string(),
            relay_url: self.state.config().effective_advertise_url(),
            server_id: Some(server_id),
            server_instance_id: Some(server_instance_id),
            ipc_address: Some(address.clone()),
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
            max_payload_size,
            route_uid,
            route_revision,
            locality: Locality::Local,
            registered_at: route_table.next_local_timestamp(),
        };
        if !route_table.can_register_route(&entry) {
            return Err(ControlError::OwnerMismatch);
        }
        let new_endpoint =
            UpstreamEndpointKey::from_route(&entry).ok_or(ControlError::OwnerMismatch)?;

        if let Some(required_replacement_address) = required_replacement_address {
            let Some(token) = replacement else {
                return Err(ControlError::DuplicateRoute {
                    existing_address: required_replacement_address,
                });
            };
            let token_existing_address = token.existing_address.clone();
            let evidence: OwnerReplacementEvidence = token.evidence;
            let Some(old_endpoint) = old_endpoint_for_cleanup.as_ref() else {
                return Err(ControlError::DuplicateRoute {
                    existing_address: token_existing_address,
                });
            };
            match self
                .state
                .validate_replaceable_owner_token(old_endpoint, &token.token, evidence)
            {
                Ok(()) => {}
                Err(OwnerReplaceError::StaleToken | OwnerReplaceError::NotReplaceable) => {
                    return Err(ControlError::DuplicateRoute {
                        existing_address: token_existing_address,
                    });
                }
            }
        }
        self.state.insert_owner_slot(&entry);
        route_table.register_prevalidated_route(entry.clone());
        drop(route_table);
        if let Some(old_endpoint) = old_endpoint_for_cleanup
            && old_endpoint != new_endpoint
            && let Some(client) = self
                .state
                .remove_connection_if_endpoint_unused(&old_endpoint)
        {
            close_replaced_owner_client(client);
        }
        Ok(RouteCommandResult::Registered { entry })
    }

    fn different_owner_replacement(
        &self,
        existing: &RouteEntry,
    ) -> Result<OwnerReplacementCandidate, ControlError> {
        let name = existing.name.as_str();
        let existing_address = existing.ipc_address.clone().unwrap_or_default();
        match self.state.connection_lookup(name) {
            CachedClient::Ready { endpoint, .. } => Err(ControlError::DuplicateRoute {
                existing_address: endpoint.address().to_string(),
            }),
            CachedClient::OwnerOnly { endpoint }
            | CachedClient::Evicted { endpoint }
            | CachedClient::Disconnected { endpoint } => {
                let Some(token) = self.state.owner_token(name) else {
                    return Err(ControlError::DuplicateRoute {
                        existing_address: endpoint.address().to_string(),
                    });
                };
                Ok(OwnerReplacementCandidate {
                    route_name: existing.name.clone(),
                    server_id: existing.server_id.clone().unwrap_or_default(),
                    server_instance_id: existing.server_instance_id.clone().unwrap_or_default(),
                    ipc_address: existing_address.clone(),
                    existing_address: endpoint.address().to_string(),
                    crm_ns: existing.crm_ns.clone(),
                    crm_name: existing.crm_name.clone(),
                    crm_ver: existing.crm_ver.clone(),
                    abi_hash: existing.abi_hash.clone(),
                    signature_hash: existing.signature_hash.clone(),
                    max_payload_size: existing.max_payload_size,
                    route_uid: existing.route_uid.clone(),
                    route_revision: existing.route_revision,
                    token,
                })
            }
            CachedClient::Missing => Err(ControlError::DuplicateRoute { existing_address }),
        }
    }

    fn unregister_local(
        &self,
        name: String,
        server_id: String,
    ) -> Result<RouteCommandResult, ControlError> {
        self.validate_route_name(&name)?;
        self.validate_server_id(&server_id)?;

        let (entry, removed_at, removed_revision) = {
            let mut route_table = self.state.route_table_write();
            let Some(existing) = route_table.local_route(&name) else {
                if route_table.local_tombstone_matches_server(&name, &server_id) {
                    return Ok(RouteCommandResult::AlreadyUnregistered);
                }
                return Err(ControlError::NotFound);
            };
            if existing.server_id.as_deref() != Some(server_id.as_str()) {
                return Err(ControlError::OwnerMismatch);
            }
            let (entry, removed_at, removed_revision) =
                route_table.unregister_local_route_with_tombstone(&name, &server_id);
            (entry, removed_at, removed_revision)
        };
        let Some(entry) = entry else {
            return Err(ControlError::NotFound);
        };
        let client = UpstreamEndpointKey::from_route(&entry)
            .as_ref()
            .and_then(|key| self.state.remove_connection_if_endpoint_unused(key));
        Ok(RouteCommandResult::Unregistered {
            entry,
            removed_at,
            removed_revision,
            client,
        })
    }

    fn announce_peer(
        &self,
        sender_relay_id: String,
        mut entry: RouteEntry,
    ) -> Result<RouteCommandResult, ControlError> {
        self.validate_route_name(&entry.name)?;
        self.validate_relay_id(&sender_relay_id)?;
        self.validate_relay_id(&entry.relay_id)?;
        self.validate_crm_tag(&entry.crm_ns, &entry.crm_name, &entry.crm_ver)?;
        c2_contract::validate_contract_hash("abi_hash", &entry.abi_hash).map_err(|err| {
            ControlError::ContractMismatch {
                reason: err.to_string(),
            }
        })?;
        c2_contract::validate_contract_hash("signature_hash", &entry.signature_hash).map_err(
            |err| ControlError::ContractMismatch {
                reason: err.to_string(),
            },
        )?;
        if !entry.registered_at.is_finite() {
            return Err(ControlError::ContractMismatch {
                reason: "route registered_at must be finite".to_string(),
            });
        }
        if !self.trusted_peer_owner(&sender_relay_id, &entry.relay_id) {
            return Err(ControlError::OwnerMismatch);
        }
        let mut route_table = self.state.route_table_write();
        if entry.relay_id == route_table.relay_id() {
            return Err(ControlError::OwnerMismatch);
        }
        let peer_url = route_table
            .get_peer(&entry.relay_id)
            .map(|peer| peer.url.clone())
            .ok_or(ControlError::OwnerMismatch)?;
        entry.relay_url = peer_url;
        entry.server_id = None;
        entry.server_instance_id = None;
        entry.ipc_address = None;
        entry.locality = Locality::Peer;
        route_table.register_route(entry);
        Ok(RouteCommandResult::PeerRouteChanged)
    }

    fn withdraw_peer(
        &self,
        sender_relay_id: String,
        name: String,
        relay_id: String,
        removed_at: f64,
        removed_revision: u64,
    ) -> Result<RouteCommandResult, ControlError> {
        self.validate_route_name(&name)?;
        self.validate_relay_id(&sender_relay_id)?;
        self.validate_relay_id(&relay_id)?;
        if !self.trusted_peer_owner(&sender_relay_id, &relay_id) {
            return Err(ControlError::OwnerMismatch);
        }
        if relay_id == self.state.relay_id() {
            return Err(ControlError::OwnerMismatch);
        }
        self.state
            .route_table_write()
            .unregister_route_with_tombstone(&name, &relay_id, removed_at, removed_revision);
        Ok(RouteCommandResult::PeerRouteChanged)
    }

    fn remove_peer_routes(&self, relay_id: &str) {
        if self.validate_relay_id(relay_id).is_err() || relay_id == self.state.relay_id() {
            return;
        }
        self.state
            .route_table_write()
            .remove_routes_by_relay(relay_id);
    }

    fn trusted_peer_owner(&self, sender_relay_id: &str, relay_id: &str) -> bool {
        sender_relay_id == relay_id
            && sender_relay_id != self.state.relay_id()
            && self.state.peer_is_alive(sender_relay_id)
    }

    async fn probe_captured_owner(&self, replacement: &OwnerReplacementCandidate) -> OwnerProbe {
        let mut client = IpcClient::new(&replacement.existing_address);
        match client.connect().await {
            Ok(()) => {
                let identity_matches = client.server_id() == Some(replacement.server_id.as_str())
                    && client.server_instance_id() == Some(replacement.server_instance_id.as_str());
                let route_matches = identity_matches
                    && client
                        .route_contract(&replacement.route_name)
                        .is_some_and(|contract| {
                            contract.route_name == replacement.route_name
                                && contract.crm_ns == replacement.crm_ns
                                && contract.crm_name == replacement.crm_name
                                && contract.crm_ver == replacement.crm_ver
                                && contract.abi_hash == replacement.abi_hash
                                && contract.signature_hash == replacement.signature_hash
                        });
                client.close().await;

                if !route_matches {
                    OwnerProbe::RouteMissing
                } else if self
                    .state
                    .matches_owner_token(&replacement.route_name, &replacement.token)
                {
                    OwnerProbe::Alive
                } else {
                    OwnerProbe::Stale
                }
            }
            Err(_) => OwnerProbe::Dead,
        }
    }
}

enum OwnerProbe {
    Alive,
    RouteMissing,
    Dead,
    Stale,
}

fn close_replaced_owner_client(client: Arc<IpcClient>) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move { client.close_shared().await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use c2_config::RelayConfig;

    use crate::relay::peer::PeerEnvelope;
    use crate::relay::types::{PeerInfo, PeerSnapshot, PeerStatus};

    struct NullDisseminator;

    impl crate::relay::disseminator::Disseminator for NullDisseminator {
        fn broadcast(
            &self,
            _envelope: PeerEnvelope,
            _peers: &[PeerSnapshot],
        ) -> Option<tokio::task::JoinHandle<()>> {
            None
        }
    }

    fn test_state() -> RelayState {
        RelayState::new(
            Arc::new(RelayConfig {
                relay_id: "relay-a".into(),
                advertise_url: "http://relay-a:8080".into(),
                ..Default::default()
            }),
            Arc::new(NullDisseminator),
        )
    }

    fn source_between<'a>(source: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let start_idx = source.find(start)?;
        let after_start = &source[start_idx..];
        let end_idx = after_start.find(end)?;
        Some(&after_start[..end_idx])
    }

    #[test]
    fn preliminary_registration_paths_do_not_attach_replacement_evidence() {
        let source = include_str!("authority.rs");
        let prepare_candidate = source_between(
            source,
            "pub(crate) async fn prepare_candidate_registration",
            "pub(crate) async fn prepare_register",
        )
        .expect("prepare_candidate_registration body should be found");
        let prepare_register = source_between(
            source,
            "pub(crate) async fn prepare_register",
            "pub(crate) async fn confirm_replacement_for_commit",
        )
        .expect("prepare_register body should be found");

        assert!(
            !prepare_candidate.contains(".with_evidence("),
            "preliminary command-loop preparation must not create commit evidence"
        );
        assert!(
            !prepare_candidate.contains("OwnerReplacementEvidence::"),
            "preliminary command-loop preparation must not name commit evidence"
        );
        assert!(
            !prepare_register.contains(".with_evidence("),
            "preliminary HTTP preparation must not create commit evidence"
        );
        assert!(
            !prepare_register.contains("OwnerReplacementEvidence::"),
            "preliminary HTTP preparation must not name commit evidence"
        );
    }

    #[test]
    fn replacement_proof_types_do_not_expose_fields() {
        let source = include_str!("authority.rs");
        let owner_replacement = source_between(
            source,
            "pub(crate) struct OwnerReplacement {",
            "pub(crate) struct OwnerReplacementCandidate {",
        )
        .expect("OwnerReplacement definition should be found");
        assert!(
            !owner_replacement
                .lines()
                .skip(1)
                .any(|line| line.trim_start().starts_with("pub ")),
            "OwnerReplacement fields must stay private so evidence cannot be constructed outside authority"
        );
        let candidate = source_between(
            source,
            "pub(crate) struct OwnerReplacementCandidate {",
            "impl OwnerReplacementCandidate",
        )
        .expect("OwnerReplacementCandidate definition should be found");
        assert!(
            !candidate
                .lines()
                .skip(1)
                .any(|line| line.trim_start().starts_with("pub ")),
            "OwnerReplacementCandidate fields must stay private so callers cannot forge captured owner state"
        );
    }

    fn peer_route(name: &str, relay_id: &str) -> RouteEntry {
        RouteEntry {
            name: name.into(),
            relay_id: relay_id.into(),
            relay_url: format!("http://{relay_id}:8080"),
            server_id: None,
            server_instance_id: None,
            ipc_address: None,
            crm_ns: "test.ns".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .into(),
            max_payload_size: 1024,
            route_uid: format!("{name}-{relay_id}-uid"),
            route_revision: 1,
            locality: Locality::Peer,
            registered_at: 1000.0,
        }
    }

    #[test]
    fn authority_rejects_invalid_relay_id_before_peer_route_mutation() {
        let state = test_state();
        state.register_peer(PeerInfo {
            relay_id: "bad/relay".into(),
            url: "http://bad-relay:8080".into(),
            route_count: 0,
            last_heartbeat: Instant::now(),
            status: PeerStatus::Alive,
        });

        let result = RouteAuthority::new(&state).execute(RouteCommand::AnnouncePeer {
            sender_relay_id: "bad/relay".into(),
            entry: Box::new(peer_route("grid", "bad/relay")),
        });

        match result {
            Err(ControlError::OwnerMismatch) => {}
            other => panic!(
                "expected invalid relay id to be rejected before mutation, got {:?}",
                other.err()
            ),
        }
        assert!(state.resolve("grid").is_empty());
    }
}
