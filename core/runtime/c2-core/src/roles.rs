//! Per-role lazily frozen capacity domains.
//!
//! One `Runtime` owns two completely independent role domains: the `client`
//! domain (frozen by the first valid remote business connection or call) and
//! the `server` domain (frozen when a `Host` starts). Neither domain's
//! creation, resolution or freeze touches the other, so a composite process
//! — one that both serves upstream callers and calls downstream resources —
//! never lets the two roles contend for the same admission slots or memory
//! budget. Each role captures its environment catalog exactly once, at its
//! own freeze; every later comparison re-evaluates against that captured
//! catalog so a frozen domain's truth never moves underneath accepted
//! configuration.

use c2_config::{
    ConfigField, ConfigResolver, ConfigSources, DomainLimits, DomainLimitsOverrides,
    DomainLimitsPatch, EnvCatalog, ResolvedServerConfig, ServerConfigOverrides, ServerConfigPatch,
    ServerIpcConfig,
};
use c2_mem::{DomainBudget, DomainObserverWeak, DomainSnapshot};

use crate::LifecycleError;

fn load_catalog() -> Result<EnvCatalog, LifecycleError> {
    EnvCatalog::load(ConfigSources::from_process())
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

fn config_error(error: c2_config::ConfigError) -> LifecycleError {
    LifecycleError::Configuration(error.to_string())
}

/// Read-only observation extracted from one role domain.
pub(crate) struct RoleDomainStats {
    pub generation: u64,
    pub snapshot: Option<DomainSnapshot>,
}

/// One role's capacity state: accepted code overrides, the catalog captured
/// at freeze, the frozen limits, and the accounting budget itself.
#[derive(Debug, Default)]
pub(crate) struct RoleDomain {
    overrides: DomainLimitsOverrides,
    /// Catalog captured at freeze; later idempotence comparisons resolve
    /// against this snapshot instead of re-reading the environment.
    catalog: Option<EnvCatalog>,
    limits: Option<DomainLimits>,
    budget: Option<DomainBudget>,
    generation: u64,
}

impl RoleDomain {
    pub(crate) fn new(overrides: DomainLimitsOverrides) -> Self {
        Self {
            overrides,
            catalog: None,
            limits: None,
            budget: None,
            generation: 0,
        }
    }

    pub(crate) fn frozen(&self) -> bool {
        self.budget.is_some()
    }

    pub(crate) fn budget(&self) -> Option<&DomainBudget> {
        self.budget.as_ref()
    }

    pub(crate) fn catalog_ref(&self) -> Option<&EnvCatalog> {
        self.catalog.as_ref()
    }

    /// The accepted code overrides, without resolving defaults or environment.
    pub(crate) fn overrides(&self) -> &DomainLimitsOverrides {
        &self.overrides
    }

    /// Tri-state incremental update. `Keep` retains the previously accepted
    /// override of a field, `Inherit` clears it back to environment/defaults,
    /// `Set` replaces it. The complete candidate resolves and validates before
    /// anything is committed; one invalid field changes nothing.
    ///
    /// After the role is frozen the candidate resolves against the captured
    /// catalog: a fully identical effective configuration is an idempotent
    /// success (the accepted overrides are still recorded), any difference is
    /// `LifecycleError::ConfigFrozen`.
    pub(crate) fn update(&mut self, patch: DomainLimitsPatch) -> Result<(), LifecycleError> {
        let candidate = self.overrides.patched(patch);
        if let (Some(catalog), Some(limits)) = (self.catalog.as_ref(), self.limits) {
            let resolved =
                ConfigResolver::resolve_domain_limits(&candidate, catalog).map_err(config_error)?;
            if resolved != limits {
                return Err(LifecycleError::ConfigFrozen);
            }
            self.overrides = candidate;
            return Ok(());
        }
        // Pre-freeze validation resolves the complete candidate truthfully
        // against the current environment; nothing is frozen yet.
        let catalog = load_catalog()?;
        ConfigResolver::resolve_domain_limits(&candidate, &catalog).map_err(config_error)?;
        self.overrides = candidate;
        Ok(())
    }

    /// Freeze against a caller-loaded catalog (the role's single activation
    /// read) and mint the budget.
    fn freeze_with(&mut self, catalog: EnvCatalog, limits: DomainLimits) -> DomainBudget {
        let budget = DomainBudget::from_limits(&limits);
        self.catalog = Some(catalog);
        self.limits = Some(limits);
        self.budget = Some(budget.clone());
        budget
    }

    /// Freeze the client role: capture the catalog once, resolve once, mint
    /// the budget. An already frozen role returns its budget unchanged.
    pub(crate) fn freeze(&mut self) -> Result<DomainBudget, LifecycleError> {
        if let Some(budget) = &self.budget {
            return Ok(budget.clone());
        }
        let catalog = load_catalog()?;
        self.freeze_with_catalog(catalog)
    }

    pub(crate) fn freeze_with_catalog(
        &mut self,
        catalog: EnvCatalog,
    ) -> Result<DomainBudget, LifecycleError> {
        if let Some(budget) = &self.budget {
            return Ok(budget.clone());
        }
        let limits = ConfigResolver::resolve_domain_limits(&self.overrides, &catalog)
            .map_err(config_error)?;
        Ok(self.freeze_with(catalog, limits))
    }

    /// Adopt another session's explicit override intent. Used by session
    /// rotation handoffs; never freezes or resolves.
    pub(crate) fn adopt_overrides(&mut self, overrides: DomainLimitsOverrides) {
        self.overrides = overrides;
    }

    /// Retire the role into a weak observer without closing its operations.
    /// Real in-flight charges stay observable; only permanent Runtime
    /// retirement calls [`Self::close_operations`].
    pub(crate) fn retire_observer(&self, role: crate::RoleKind) -> Option<RetiredRoleObserver> {
        let observer = self.budget.as_ref()?.observe();
        Some(RetiredRoleObserver {
            role,
            generation: self.generation,
            domain_id: observer.snapshot().domain_id,
            observer: observer.downgrade(),
        })
    }

    /// Permanent retirement fence: stop new-call admission for this role.
    pub(crate) fn close_operations(&self) {
        if let Some(budget) = &self.budget {
            budget.close_operations();
        }
    }

    /// Read-only projection; never resolves, creates a domain, or reads the
    /// environment.
    pub(crate) fn stats(&self) -> RoleDomainStats {
        let snapshot = self.budget.as_ref().map(DomainBudget::snapshot);
        RoleDomainStats {
            generation: self.generation,
            snapshot,
        }
    }

    /// Advance the generation when a Host restart reuses this budget.
    pub(crate) fn advance_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

/// The complete frozen server configuration plus the concrete identity an
/// auto-generated server id received at freeze.
#[derive(Debug, Clone)]
pub(crate) struct FrozenServerConfig {
    pub server_id: Option<String>,
    pub startup_timeout: std::time::Duration,
    pub ipc: ServerIpcConfig,
    pub domain_limits: DomainLimits,
}

impl PartialEq for FrozenServerConfig {
    fn eq(&self, other: &Self) -> bool {
        self.server_id == other.server_id
            && self.startup_timeout == other.startup_timeout
            && self.ipc == other.ipc
            && self.domain_limits == other.domain_limits
    }
}
impl Eq for FrozenServerConfig {}

impl FrozenServerConfig {
    fn from_resolved(resolved: &ResolvedServerConfig) -> Self {
        Self {
            server_id: resolved.server_id.clone(),
            startup_timeout: resolved.startup_timeout,
            ipc: resolved.ipc,
            domain_limits: resolved.domain_limits,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostSettings {
    pub relay_anchor_address: Option<String>,
    pub relay_use_proxy: bool,
    pub shutdown_timeout: std::time::Duration,
}

/// The result of freezing the server role for one Host start.
pub(crate) struct ServerFreeze {
    pub config: FrozenServerConfig,
    pub budget: DomainBudget,
    pub settings: HostSettings,
    /// True when the role was already frozen and this start is a restart.
    pub restart: bool,
}

/// The server role extends the capacity domain with the full Host identity
/// and carrying-capacity configuration that freezes together with it.
#[derive(Debug, Default)]
pub(crate) struct ServerRole {
    domain: RoleDomain,
    server_overrides: ServerConfigOverrides,
    /// The resolved configuration frozen by the running Host, with the
    /// concrete server identity materialized.
    resolved: Option<FrozenServerConfig>,
    host_settings: Option<HostSettings>,
    auto_identity: bool,
}

impl ServerRole {
    pub(crate) fn new(server_overrides: ServerConfigOverrides) -> Self {
        Self {
            domain: RoleDomain::new(server_overrides.domain_limits),
            server_overrides,
            resolved: None,
            host_settings: None,
            auto_identity: false,
        }
    }

    pub(crate) fn frozen(&self) -> bool {
        self.domain.frozen()
    }

    pub(crate) fn budget(&self) -> Option<&DomainBudget> {
        self.domain.budget()
    }

    pub(crate) fn resolved(&self) -> Option<&FrozenServerConfig> {
        self.resolved.as_ref()
    }

    pub(crate) fn overrides(&self) -> &ServerConfigOverrides {
        &self.server_overrides
    }

    fn resolve(
        overrides: &ServerConfigOverrides,
        catalog: &EnvCatalog,
    ) -> Result<FrozenServerConfig, LifecycleError> {
        let resolved = ConfigResolver::resolve_server(overrides, catalog).map_err(config_error)?;
        Ok(FrozenServerConfig::from_resolved(&resolved))
    }

    fn align_auto_identity(
        &self,
        mut resolved: FrozenServerConfig,
        frozen: &FrozenServerConfig,
    ) -> FrozenServerConfig {
        if self.auto_identity && resolved.server_id.is_none() {
            // An auto-generated identity follows the captured one unless the
            // patch names a new explicit server id.
            resolved.server_id = frozen.server_id.clone();
        }
        resolved
    }

    /// Tri-state incremental update across the complete server role. The
    /// candidate resolves atomically; once frozen it resolves against the
    /// captured catalog, where identical effective values (auto identity
    /// aligned) stay idempotent and any difference is `ConfigFrozen`.
    pub(crate) fn update(&mut self, patch: ServerConfigPatch) -> Result<(), LifecycleError> {
        let candidate = self.server_overrides.clone().patched(patch);
        if let Some(frozen) = &self.resolved {
            let catalog = self
                .domain
                .catalog_ref()
                .expect("frozen server role captured its catalog");
            let resolved = Self::resolve(&candidate, catalog)?;
            if self.align_auto_identity(resolved, frozen) != *frozen {
                return Err(LifecycleError::ConfigFrozen);
            }
            let limits = candidate.domain_limits;
            self.server_overrides = candidate;
            self.domain.adopt_overrides(limits);
            return Ok(());
        }
        let catalog = load_catalog()?;
        Self::resolve(&candidate, &catalog)?;
        let limits = candidate.domain_limits;
        self.server_overrides = candidate;
        self.domain.adopt_overrides(limits);
        Ok(())
    }

    /// Freeze the server role for one Host start: one catalog load resolves
    /// the complete candidate and freezes the domain. A frozen role accepts
    /// the start idempotently only when the candidate's complete effective
    /// configuration (auto identity aligned) equals the frozen one; the
    /// budget is reused with its real in-flight accounting intact.
    pub(crate) fn start(
        &mut self,
        patch: ServerConfigPatch,
        auto_identity: impl FnOnce() -> String,
        settings: impl FnOnce(
            &EnvCatalog,
            Option<&HostSettings>,
        ) -> Result<HostSettings, LifecycleError>,
    ) -> Result<ServerFreeze, LifecycleError> {
        let candidate = self.server_overrides.clone().patched(patch);
        if let Some(frozen) = &self.resolved {
            let catalog = self
                .catalog_ref()
                .expect("frozen server role captured its catalog");
            let resolved = Self::resolve(&candidate, catalog)?;
            let host_settings = settings(catalog, self.host_settings.as_ref())?;
            if self.align_auto_identity(resolved, frozen) != *frozen
                || Some(&host_settings) != self.host_settings.as_ref()
            {
                return Err(LifecycleError::ConfigFrozen);
            }
            let frozen = frozen.clone();
            self.adopt_overrides(candidate);
            return Ok(ServerFreeze {
                config: frozen,
                budget: self
                    .domain
                    .budget()
                    .expect("frozen server role owns its budget")
                    .clone(),
                settings: host_settings,
                restart: true,
            });
        }
        let catalog = load_catalog()?;
        let mut resolved = Self::resolve(&candidate, &catalog)?;
        let host_settings = settings(&catalog, None)?;
        self.auto_identity = resolved.server_id.is_none();
        if self.auto_identity {
            resolved.server_id = Some(auto_identity());
        }
        let budget = self.domain.freeze_with(catalog, resolved.domain_limits);
        self.adopt_overrides(candidate);
        self.resolved = Some(resolved.clone());
        self.host_settings = Some(host_settings.clone());
        Ok(ServerFreeze {
            config: resolved,
            budget,
            settings: host_settings,
            restart: false,
        })
    }

    pub(crate) fn catalog_ref(&self) -> Option<&EnvCatalog> {
        self.domain.catalog_ref()
    }

    pub(crate) fn host_settings(&self) -> Option<&HostSettings> {
        self.host_settings.as_ref()
    }

    /// Adopt another session's explicit server override intent. An
    /// auto-generated server id never travels as an explicit override.
    pub(crate) fn adopt_overrides(&mut self, overrides: ServerConfigOverrides) {
        let limits = overrides.domain_limits;
        self.server_overrides = overrides;
        self.domain.adopt_overrides(limits);
    }

    pub(crate) fn retire_observer(&self) -> Option<RetiredRoleObserver> {
        self.domain.retire_observer(crate::RoleKind::Server)
    }

    pub(crate) fn close_operations(&self) {
        self.domain.close_operations();
    }

    pub(crate) fn stats(&self) -> RoleDomainStats {
        self.domain.stats()
    }

    pub(crate) fn advance_generation(&mut self) {
        self.domain.advance_generation();
    }
}

/// The Runtime-facing client-role patch: only capacity fields are updatable
/// before the client domain freezes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientConfigPatch {
    pub domain_limits: DomainLimitsPatch,
}

impl ClientConfigPatch {
    /// Clear every field's code override and return to environment/defaults.
    pub fn inherit_all() -> Self {
        Self {
            domain_limits: DomainLimitsPatch {
                memory_budget_bytes: ConfigField::Inherit,
                file_spill_budget_bytes: ConfigField::Inherit,
                max_outstanding_calls: ConfigField::Inherit,
            },
        }
    }
}

/// A weak handle keeping one retired role's accounting observable.
#[derive(Debug, Clone)]
pub struct RetiredRoleObserver {
    pub role: crate::RoleKind,
    pub generation: u64,
    domain_id: u64,
    observer: DomainObserverWeak,
}

impl RetiredRoleObserver {
    /// Snapshot the retired accounting, or `None` once every real owner of
    /// the domain is gone. Pure read-only.
    pub fn snapshot(&self) -> Option<DomainSnapshot> {
        self.observer.snapshot()
    }

    /// Whether a real owner still keeps this domain's accounting alive.
    pub fn is_alive(&self) -> bool {
        self.observer.is_alive()
    }

    /// Identity for adoption dedup: the same role generation of the same
    /// accounting domain (domain ids are unique per minted budget).
    pub(crate) fn same_role_domain(&self, other: &Self) -> bool {
        self.domain_id == other.domain_id
            && self.role == other.role
            && self.generation == other.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_config::{EnvFilePolicy, EnvMap};

    #[test]
    fn frozen_domain_inherits_from_captured_catalog_and_rejects_partial_updates() {
        let catalog = EnvCatalog::load(ConfigSources {
            env_file: EnvFilePolicy::Disabled,
            process_env: EnvMap::from([
                ("C2_MEMORY_BUDGET_BYTES".into(), "12345".into()),
                ("C2_FILE_SPILL_BUDGET_BYTES".into(), "6789".into()),
                ("C2_CALL_MAX_OUTSTANDING".into(), "2".into()),
            ]),
        })
        .unwrap();
        let mut role = RoleDomain::new(DomainLimitsOverrides {
            memory_budget_bytes: Some(12345),
            file_spill_budget_bytes: None,
            max_outstanding_calls: Some(3),
        });
        let limits = ConfigResolver::resolve_domain_limits(role.overrides(), &catalog).unwrap();
        let budget = role.freeze_with(catalog, limits);
        let charge = budget.try_reserve_memory(16).unwrap();
        let before = budget.snapshot();

        role.update(DomainLimitsPatch {
            memory_budget_bytes: ConfigField::Inherit,
            file_spill_budget_bytes: ConfigField::Set(6789),
            max_outstanding_calls: ConfigField::Keep,
        })
        .unwrap();

        let accepted = DomainLimitsOverrides {
            memory_budget_bytes: None,
            file_spill_budget_bytes: Some(6789),
            max_outstanding_calls: Some(3),
        };
        assert_eq!(*role.overrides(), accepted);
        assert_eq!(role.stats().snapshot, Some(before));

        assert_eq!(
            role.update(DomainLimitsPatch {
                max_outstanding_calls: ConfigField::Inherit,
                ..Default::default()
            }),
            Err(LifecycleError::ConfigFrozen)
        );
        assert!(matches!(
            role.update(DomainLimitsPatch {
                memory_budget_bytes: ConfigField::Set(0),
                file_spill_budget_bytes: ConfigField::Inherit,
                max_outstanding_calls: ConfigField::Keep,
            }),
            Err(LifecycleError::Configuration(_))
        ));
        assert_eq!(*role.overrides(), accepted);
        assert_eq!(role.stats().snapshot, Some(before));
        drop(charge);
        assert_eq!(role.stats().snapshot.unwrap().memory.used_bytes, 0);
    }
}
