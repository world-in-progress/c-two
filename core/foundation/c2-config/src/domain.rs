//! Capacity and code-level overrides for one client, Host or relay domain.

use crate::{ConfigError, ConfigResolver, EnvCatalog};

pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 16 * (1 << 30);
pub const DEFAULT_FILE_SPILL_BUDGET_BYTES: u64 = 16 * (1 << 30);

pub const fn default_max_outstanding_calls(memory_budget_bytes: u64) -> u64 {
    if memory_budget_bytes == 0 {
        return 0;
    }
    let calls = memory_budget_bytes / 65_536;
    if calls < 64 { 64 } else { calls }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigField<T> {
    #[default]
    Keep,
    Inherit,
    Set(T),
}

impl<T> ConfigField<T> {
    pub fn apply(self, target: &mut Option<T>) {
        match self {
            Self::Keep => {}
            Self::Inherit => *target = None,
            Self::Set(value) => *target = Some(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainLimits {
    pub memory_budget_bytes: u64,
    pub file_spill_budget_bytes: u64,
    pub max_outstanding_calls: u64,
}

impl Default for DomainLimits {
    fn default() -> Self {
        Self {
            memory_budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
            file_spill_budget_bytes: DEFAULT_FILE_SPILL_BUDGET_BYTES,
            max_outstanding_calls: default_max_outstanding_calls(DEFAULT_MEMORY_BUDGET_BYTES),
        }
    }
}

impl DomainLimits {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.memory_budget_bytes == 0 && self.max_outstanding_calls > 0 {
            return Err(ConfigError::new(
                "max_outstanding_calls (C2_CALL_MAX_OUTSTANDING) must be zero when memory_budget_bytes (C2_MEMORY_BUDGET_BYTES) is zero",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DomainLimitsPatch {
    pub memory_budget_bytes: ConfigField<u64>,
    pub file_spill_budget_bytes: ConfigField<u64>,
    pub max_outstanding_calls: ConfigField<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DomainLimitsOverrides {
    pub memory_budget_bytes: Option<u64>,
    pub file_spill_budget_bytes: Option<u64>,
    pub max_outstanding_calls: Option<u64>,
}

impl DomainLimitsOverrides {
    /// Form a candidate for validation together with the rest of a role's config.
    pub fn patched(mut self, patch: DomainLimitsPatch) -> Self {
        patch
            .memory_budget_bytes
            .apply(&mut self.memory_budget_bytes);
        patch
            .file_spill_budget_bytes
            .apply(&mut self.file_spill_budget_bytes);
        patch
            .max_outstanding_calls
            .apply(&mut self.max_outstanding_calls);
        self
    }

    /// Commit only a candidate that resolves successfully against this catalog.
    pub fn apply_patch(
        &mut self,
        patch: DomainLimitsPatch,
        catalog: &EnvCatalog,
    ) -> Result<DomainLimits, ConfigError> {
        let candidate = self.patched(patch);
        let resolved = ConfigResolver::resolve_domain_limits(&candidate, catalog)?;
        *self = candidate;
        Ok(resolved)
    }
}
