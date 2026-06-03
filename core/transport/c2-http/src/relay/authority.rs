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
use crate::relay::types::{Locality, RouteEntry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlError {
    InvalidName { reason: String },
    InvalidServerId { reason: String },
    InvalidServerInstanceId { reason: String },
    InvalidAddress { reason: String },
    ContractMismatch { reason: String },
    AddressMismatch { existing_address: String },
    DuplicateRoute { existing_address: String },
    OwnerMismatch,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttestedRouteContract {
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub max_payload_size: u64,
}

fn existing_contract_mismatch_reason(
    route_name: &str,
    existing: &RouteEntry,
    crm_ns: &str,
    crm_name: &str,
    crm_ver: &str,
    abi_hash: &str,
    signature_hash: &str,
    max_payload_size: u64,
) -> Option<String> {
    if existing.crm_ns != crm_ns
        || existing.crm_name != crm_name
        || existing.crm_ver != crm_ver
        || existing.abi_hash != abi_hash
        || existing.signature_hash != signature_hash
        || existing.max_payload_size != max_payload_size
    {
        return Some(format!(
            "CRM contract mismatch for route '{route_name}': existing {}/{}/{} hashes={}/{} max_payload_size={}, got {}/{}/{} hashes={}/{} max_payload_size={}",
            existing.crm_ns,
            existing.crm_name,
            existing.crm_ver,
            existing.abi_hash,
            existing.signature_hash,
            existing.max_payload_size,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
            max_payload_size
        ));
    }
    None
}

pub(crate) fn read_ipc_route_contract(
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
    Ok(AttestedRouteContract {
        crm_ns: contract.crm_ns,
        crm_name: contract.crm_name,
        crm_ver: contract.crm_ver,
        abi_hash: contract.abi_hash,
        signature_hash: contract.signature_hash,
        max_payload_size: client
            .route_max_payload_size(route_name)
            .ok_or(ControlError::NotFound)?,
    })
}

pub(crate) fn attest_ipc_route_contract(
    client: &IpcClient,
    route_name: &str,
    claimed_crm_ns: &str,
    claimed_crm_name: &str,
    claimed_crm_ver: &str,
    claimed_abi_hash: &str,
    claimed_signature_hash: &str,
    claimed_max_payload_size: u64,
) -> Result<AttestedRouteContract, ControlError> {
    let contract = client
        .route_contract(route_name)
        .ok_or(ControlError::NotFound)?;
    read_ipc_route_contract(client, route_name)?;
    let claimed = ExpectedRouteContract {
        route_name: route_name.to_string(),
        crm_ns: claimed_crm_ns.to_string(),
        crm_name: claimed_crm_name.to_string(),
        crm_ver: claimed_crm_ver.to_string(),
        abi_hash: claimed_abi_hash.to_string(),
        signature_hash: claimed_signature_hash.to_string(),
    };
    if c2_contract::validate_expected_route_contract(&claimed).is_err() {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream route '{route_name}' registration did not claim a complete CRM contract"
            ),
        });
    }
    if claimed != contract {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream route '{route_name}' CRM contract mismatch: claimed {}/{}/{} hashes={}/{}, got {}/{}/{} hashes={}/{}",
                claimed.crm_ns,
                claimed.crm_name,
                claimed.crm_ver,
                claimed.abi_hash,
                claimed.signature_hash,
                contract.crm_ns,
                contract.crm_name,
                contract.crm_ver,
                contract.abi_hash,
                contract.signature_hash,
            ),
        });
    }
    let max_payload_size = client
        .route_max_payload_size(route_name)
        .ok_or(ControlError::NotFound)?;
    if claimed_max_payload_size != max_payload_size {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream route '{route_name}' max_payload_size mismatch: claimed {claimed_max_payload_size}, got {max_payload_size}"
            ),
        });
    }
    Ok(AttestedRouteContract {
        crm_ns: contract.crm_ns,
        crm_name: contract.crm_name,
        crm_ver: contract.crm_ver,
        abi_hash: contract.abi_hash,
        signature_hash: contract.signature_hash,
        max_payload_size,
    })
}

pub(crate) async fn attest_ipc_pending_route_contract(
    client: &mut IpcClient,
    route_name: &str,
    registration_token: &str,
    claimed_crm_ns: &str,
    claimed_crm_name: &str,
    claimed_crm_ver: &str,
    claimed_abi_hash: &str,
    claimed_signature_hash: &str,
    claimed_max_payload_size: u64,
) -> Result<AttestedRouteContract, ControlError> {
    let contract = match client
        .pending_route_contract(route_name, registration_token)
        .await
    {
        Ok(contract) => contract,
        Err(c2_ipc::IpcError::RouteNotFound(_)) => return Err(ControlError::NotFound),
        Err(err) => {
            return Err(ControlError::ContractMismatch {
                reason: format!(
                    "IPC upstream pending route '{route_name}' attestation failed: {err}"
                ),
            });
        }
    };
    let claimed = ExpectedRouteContract {
        route_name: route_name.to_string(),
        crm_ns: claimed_crm_ns.to_string(),
        crm_name: claimed_crm_name.to_string(),
        crm_ver: claimed_crm_ver.to_string(),
        abi_hash: claimed_abi_hash.to_string(),
        signature_hash: claimed_signature_hash.to_string(),
    };
    let max_payload_size = client
        .route_max_payload_size(route_name)
        .ok_or(ControlError::NotFound)?;
    if c2_contract::validate_expected_route_contract(&claimed).is_err() {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream route '{route_name}' registration did not claim a complete CRM contract"
            ),
        });
    }
    if claimed != contract {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream pending route '{route_name}' CRM contract mismatch: claimed {}/{}/{} hashes={}/{}, got {}/{}/{} hashes={}/{}",
                claimed.crm_ns,
                claimed.crm_name,
                claimed.crm_ver,
                claimed.abi_hash,
                claimed.signature_hash,
                contract.crm_ns,
                contract.crm_name,
                contract.crm_ver,
                contract.abi_hash,
                contract.signature_hash,
            ),
        });
    }
    if claimed_max_payload_size != max_payload_size {
        return Err(ControlError::ContractMismatch {
            reason: format!(
                "IPC upstream pending route '{route_name}' max_payload_size mismatch: claimed {claimed_max_payload_size}, got {max_payload_size}"
            ),
        });
    }
    Ok(AttestedRouteContract {
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
        replacement: Option<OwnerReplacementCandidate>,
    },
    SameOwner,
}

pub(crate) enum RegisterPreparation {
    Available {
        replacement: Option<OwnerReplacementCandidate>,
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
            token: self.token,
            evidence,
        }
    }
}

pub(crate) enum RouteCommand {
    RegisterLocal {
        name: String,
        server_id: String,
        server_instance_id: String,
        address: String,
        crm_ns: String,
        crm_name: String,
        crm_ver: String,
        abi_hash: String,
        signature_hash: String,
        max_payload_size: u64,
        replacement: Option<OwnerReplacement>,
    },
    UnregisterLocal {
        name: String,
        server_id: String,
    },
    AnnouncePeer {
        sender_relay_id: String,
        entry: RouteEntry,
    },
    WithdrawPeer {
        sender_relay_id: String,
        name: String,
        relay_id: String,
        removed_at: f64,
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
        c2_ipc::socket_path_from_ipc_address(address)
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
            replacement: Some(replacement),
        })
    }

    pub(crate) async fn prepare_candidate_registration(
        &self,
        name: &str,
        server_id: &str,
        address: &str,
    ) -> Result<Option<OwnerReplacementCandidate>, ControlError> {
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
                OwnerProbe::RouteMissing | OwnerProbe::Dead => return Ok(Some(replacement)),
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
        replacement: Option<OwnerReplacementCandidate>,
    ) -> Result<Option<OwnerReplacement>, ControlError> {
        let Some(replacement) = replacement else {
            return Ok(None);
        };

        match self.probe_captured_owner(&replacement).await {
            OwnerProbe::Alive => Err(ControlError::DuplicateRoute {
                existing_address: replacement.existing_address,
            }),
            OwnerProbe::RouteMissing => Ok(Some(
                replacement.with_evidence(OwnerReplacementEvidence::ConfirmedRouteMissing),
            )),
            OwnerProbe::Dead => Ok(Some(
                replacement.with_evidence(OwnerReplacementEvidence::ConfirmedDead),
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
            RouteCommand::RegisterLocal {
                name,
                server_id,
                server_instance_id,
                address,
                crm_ns,
                crm_name,
                crm_ver,
                abi_hash,
                signature_hash,
                max_payload_size,
                replacement,
            } => self.register_local(
                name,
                server_id,
                server_instance_id,
                address,
                crm_ns,
                crm_name,
                crm_ver,
                abi_hash,
                signature_hash,
                max_payload_size,
                replacement,
            ),
            RouteCommand::UnregisterLocal { name, server_id } => {
                self.unregister_local(name, server_id)
            }
            RouteCommand::AnnouncePeer {
                sender_relay_id,
                entry,
            } => self.announce_peer(sender_relay_id, entry),
            RouteCommand::WithdrawPeer {
                sender_relay_id,
                name,
                relay_id,
                removed_at,
            } => self.withdraw_peer(sender_relay_id, name, relay_id, removed_at),
            RouteCommand::RemovePeerRoutes { relay_id } => {
                self.remove_peer_routes(&relay_id);
                Ok(RouteCommandResult::PeerRoutesRemoved)
            }
        }
    }

    fn register_local(
        &self,
        name: String,
        server_id: String,
        server_instance_id: String,
        address: String,
        crm_ns: String,
        crm_name: String,
        crm_ver: String,
        abi_hash: String,
        signature_hash: String,
        max_payload_size: u64,
        replacement: Option<OwnerReplacement>,
    ) -> Result<RouteCommandResult, ControlError> {
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

        let mut route_table = self.state.route_table_write();
        if let Some(existing) = route_table.local_route(&name) {
            let existing_address = existing.ipc_address.clone().unwrap_or_default();
            let existing_server_id = existing.server_id.clone().unwrap_or_default();
            let existing_server_instance_id =
                existing.server_instance_id.clone().unwrap_or_default();
            if existing_server_id == server_id {
                if existing_address == address {
                    if existing_server_instance_id == server_instance_id {
                        if let Some(reason) = existing_contract_mismatch_reason(
                            &name,
                            &existing,
                            &crm_ns,
                            &crm_name,
                            &crm_ver,
                            &abi_hash,
                            &signature_hash,
                            max_payload_size,
                        ) {
                            return Err(ControlError::ContractMismatch { reason });
                        }
                        if let Some(token) = self.state.owner_token(&name) {
                            self.state.renew_owner_lease(&name, &token);
                        }
                        return Ok(RouteCommandResult::SameOwner { entry: existing });
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
                        {
                            return Err(ControlError::DuplicateRoute { existing_address });
                        }
                    }
                    _ => return Err(ControlError::DuplicateRoute { existing_address }),
                }
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
            locality: Locality::Local,
            registered_at: route_table.next_local_timestamp(),
        };
        if !route_table.can_register_route(&entry) {
            return Err(ControlError::OwnerMismatch);
        }

        let old_client = if let Some(token) = replacement {
            let token_existing_address = token.existing_address.clone();
            let evidence: OwnerReplacementEvidence = token.evidence;
            match self
                .state
                .replace_if_owner_token(&name, &token.token, address.clone(), evidence)
            {
                Ok(old_client) => old_client,
                Err(OwnerReplaceError::StaleToken | OwnerReplaceError::NotReplaceable) => {
                    return Err(ControlError::DuplicateRoute {
                        existing_address: token_existing_address,
                    });
                }
            }
        } else {
            self.state.insert_owner_slot(name.clone(), address);
            None
        };
        route_table.register_prevalidated_route(entry.clone());
        drop(route_table);
        if let Some(client) = old_client {
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
            CachedClient::Ready { address, .. } => Err(ControlError::DuplicateRoute {
                existing_address: address,
            }),
            CachedClient::OwnerOnly { address }
            | CachedClient::Evicted { address }
            | CachedClient::Disconnected { address } => {
                let Some(token) = self.state.owner_token(name) else {
                    return Err(ControlError::DuplicateRoute {
                        existing_address: address,
                    });
                };
                Ok(OwnerReplacementCandidate {
                    route_name: existing.name.clone(),
                    server_id: existing.server_id.clone().unwrap_or_default(),
                    server_instance_id: existing.server_instance_id.clone().unwrap_or_default(),
                    ipc_address: existing_address.clone(),
                    existing_address: address,
                    crm_ns: existing.crm_ns.clone(),
                    crm_name: existing.crm_name.clone(),
                    crm_ver: existing.crm_ver.clone(),
                    abi_hash: existing.abi_hash.clone(),
                    signature_hash: existing.signature_hash.clone(),
                    max_payload_size: existing.max_payload_size,
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

        let (entry, removed_at, client) = {
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
            let (entry, removed_at) =
                route_table.unregister_local_route_with_tombstone(&name, &server_id);
            let client = self.state.remove_connection(&name);
            (entry, removed_at, client)
        };
        let Some(entry) = entry else {
            return Err(ControlError::NotFound);
        };
        Ok(RouteCommandResult::Unregistered {
            entry,
            removed_at,
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
            .unregister_route_with_tombstone(&name, &relay_id, removed_at);
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
            entry: peer_route("grid", "bad/relay"),
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
