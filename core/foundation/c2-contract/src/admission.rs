//! Bounded JSON admission policy for externally supplied contract documents.

use crate::ContractError;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
#[cfg(debug_assertions)]
use std::cell::Cell;
use std::fmt;

const V1_MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const V1_MAX_JSON_VALUES: u64 = 1_000_000;
const V1_MAX_NESTING_DEPTH: u64 = 128;
const V1_MAX_NESTED_FASTDB_BYTES: u64 = 16 * 1024 * 1024;

/// The single hard method capacity shared by contract admission and C-Two wire
/// metadata.
pub const MAX_CONTRACT_METHODS: usize = 256;

/// Versioned default-policy identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractLimitsProfile {
    /// The initial bounded contract admission profile.
    V1,
}

impl ContractLimitsProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
        }
    }
}

/// Stable resource dimension reported by [`ContractError::LimitExceeded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractLimitMetric {
    /// Exact bytes in the external JSON source.
    SourceBytes,
    /// Root plus every object-member value and array element.
    JsonValues,
    /// JSON value depth with the root counted as one.
    JsonDepth,
    /// Entries in the outer contract `methods` array.
    Methods,
    /// Canonical extracted bytes across every nested FastDB binding occurrence.
    NestedFastDbBytes,
}

impl ContractLimitMetric {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceBytes => "source_bytes",
            Self::JsonValues => "json_values",
            Self::JsonDepth => "json_depth",
            Self::Methods => "methods",
            Self::NestedFastDbBytes => "nested_fastdb_bytes",
        }
    }
}

/// Versioned control-plane admission limits.
///
/// These values never enter canonical descriptor bytes or release identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractLimits {
    profile: ContractLimitsProfile,
    max_source_bytes: u64,
    max_json_values: u64,
    max_nesting_depth: u64,
    max_methods: u64,
    max_nested_fastdb_bytes: u64,
}

impl ContractLimits {
    /// Returns the immutable V1 default table.
    pub const fn v1() -> Self {
        Self {
            profile: ContractLimitsProfile::V1,
            max_source_bytes: V1_MAX_SOURCE_BYTES,
            max_json_values: V1_MAX_JSON_VALUES,
            max_nesting_depth: V1_MAX_NESTING_DEPTH,
            max_methods: MAX_CONTRACT_METHODS as u64,
            max_nested_fastdb_bytes: V1_MAX_NESTED_FASTDB_BYTES,
        }
    }

    pub const fn profile(&self) -> ContractLimitsProfile {
        self.profile
    }

    pub const fn max_source_bytes(&self) -> u64 {
        self.max_source_bytes
    }

    pub const fn max_json_values(&self) -> u64 {
        self.max_json_values
    }

    pub const fn max_nesting_depth(&self) -> u64 {
        self.max_nesting_depth
    }

    pub const fn max_methods(&self) -> u64 {
        self.max_methods
    }

    pub const fn max_nested_fastdb_bytes(&self) -> u64 {
        self.max_nested_fastdb_bytes
    }

    /// Tightens or relaxes the source byte policy within platform allocation
    /// representability.
    pub const fn with_max_source_bytes(mut self, limit: u64) -> Self {
        self.max_source_bytes = limit;
        self
    }

    /// Tightens or relaxes the JSON value-count policy within platform
    /// allocation representability.
    pub const fn with_max_json_values(mut self, limit: u64) -> Self {
        self.max_json_values = limit;
        self
    }

    /// Tightens or relaxes the JSON depth policy within platform allocation
    /// representability.
    pub const fn with_max_nesting_depth(mut self, limit: u64) -> Self {
        self.max_nesting_depth = limit;
        self
    }

    /// Sets the method policy without allowing it to exceed the hard C-Two
    /// method capacity.
    pub const fn with_max_methods(mut self, limit: u64) -> Self {
        self.max_methods = if limit > MAX_CONTRACT_METHODS as u64 {
            MAX_CONTRACT_METHODS as u64
        } else {
            limit
        };
        self
    }

    /// Tightens or relaxes cumulative nested FastDB extraction bytes within
    /// platform allocation representability.
    pub const fn with_max_nested_fastdb_bytes(mut self, limit: u64) -> Self {
        self.max_nested_fastdb_bytes = limit;
        self
    }
}

impl Default for ContractLimits {
    fn default() -> Self {
        Self::v1()
    }
}

#[cfg(debug_assertions)]
thread_local! {
    static DESCRIPTOR_ADMISSION_COUNT: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn record_descriptor_admission() {
    #[cfg(debug_assertions)]
    DESCRIPTOR_ADMISSION_COUNT.with(|count| count.set(count.get().saturating_add(1)));
}

#[doc(hidden)]
/// Returns this thread's descriptor-admission count in debug builds.
///
/// This is an implementation-test probe, not a production metric. Release
/// builds return zero and do not maintain the counter.
pub fn descriptor_admission_count_for_current_thread() -> u64 {
    #[cfg(debug_assertions)]
    {
        DESCRIPTOR_ADMISSION_COUNT.with(Cell::get)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum JsonDocumentKind {
    ContractDescriptor,
    ContractReleaseRef,
}

impl JsonDocumentKind {
    fn invalid_json(self, message: String) -> ContractError {
        match self {
            Self::ContractDescriptor => ContractError::InvalidJson(message),
            Self::ContractReleaseRef => ContractError::InvalidReleaseRefJson(message),
        }
    }
}

pub(crate) fn parse_bounded_json(
    bytes: &[u8],
    limits: ContractLimits,
    kind: JsonDocumentKind,
) -> Result<Value, ContractError> {
    validate_json_limit_configuration(limits)?;

    let source_bytes = usize_to_u64(bytes.len());
    ensure_within_limit(
        limits,
        ContractLimitMetric::SourceBytes,
        limits.max_source_bytes(),
        source_bytes,
        "$",
    )?;

    let mut state = AdmissionState {
        limits,
        values: 0,
        limit_error: None,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    // ContractLimits is the recursion authority. The V1 guard stops a child
    // before depth 129 is handed back to serde_json.
    deserializer.disable_recursion_limit();
    let result = BoundedValueSeed {
        state: &mut state,
        depth: 1,
        path: "$".to_string(),
    }
    .deserialize(&mut deserializer)
    .and_then(|value| {
        deserializer.end()?;
        Ok(value)
    });

    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(state
            .limit_error
            .unwrap_or_else(|| kind.invalid_json(error.to_string()))),
    }
}

pub(crate) fn checked_observed_add(
    limits: ContractLimits,
    metric: ContractLimitMetric,
    limit: u64,
    current: u64,
    increment: u64,
    path: &str,
) -> Result<u64, ContractError> {
    let observed = match current.checked_add(increment) {
        Some(observed) => observed,
        None => {
            return Err(limit_exceeded(limits, metric, limit, u64::MAX, path));
        }
    };
    ensure_within_limit(limits, metric, limit, observed, path)?;
    Ok(observed)
}

pub(crate) fn validate_descriptor_limit_configuration(
    limits: ContractLimits,
) -> Result<(), ContractError> {
    validate_json_limit_configuration(limits)?;
    validate_representable_limit(
        limits,
        ContractLimitMetric::NestedFastDbBytes,
        limits.max_nested_fastdb_bytes(),
        "$limits.max_nested_fastdb_bytes",
    )
}

fn validate_json_limit_configuration(limits: ContractLimits) -> Result<(), ContractError> {
    for (metric, configured, path) in [
        (
            ContractLimitMetric::SourceBytes,
            limits.max_source_bytes(),
            "$limits.max_source_bytes",
        ),
        (
            ContractLimitMetric::JsonValues,
            limits.max_json_values(),
            "$limits.max_json_values",
        ),
        (
            ContractLimitMetric::JsonDepth,
            limits.max_nesting_depth(),
            "$limits.max_nesting_depth",
        ),
    ] {
        validate_representable_limit(limits, metric, configured, path)?;
    }
    Ok(())
}

fn validate_representable_limit(
    limits: ContractLimits,
    metric: ContractLimitMetric,
    configured: u64,
    path: &str,
) -> Result<(), ContractError> {
    // Rust allocation APIs cannot represent lengths above isize::MAX even on
    // a target whose usize is wider. Reject such policy values before parsing
    // rather than silently truncating them during later capacity decisions.
    let platform_max = usize_to_u64(isize::MAX as usize);
    ensure_within_limit(limits, metric, platform_max, configured, path)
}

fn ensure_within_limit(
    limits: ContractLimits,
    metric: ContractLimitMetric,
    limit: u64,
    observed: u64,
    path: &str,
) -> Result<(), ContractError> {
    if observed <= limit {
        return Ok(());
    }
    Err(limit_exceeded(limits, metric, limit, observed, path))
}

fn limit_exceeded(
    limits: ContractLimits,
    metric: ContractLimitMetric,
    limit: u64,
    observed: u64,
    path: &str,
) -> ContractError {
    ContractError::LimitExceeded {
        profile: limits.profile(),
        metric,
        limit,
        observed,
        path: path.to_string(),
    }
}

pub(crate) fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

struct AdmissionState {
    limits: ContractLimits,
    values: u64,
    limit_error: Option<ContractError>,
}

impl AdmissionState {
    fn observe_value(&mut self, depth: u64, path: &str) -> Result<(), ContractError> {
        self.values = checked_observed_add(
            self.limits,
            ContractLimitMetric::JsonValues,
            self.limits.max_json_values(),
            self.values,
            1,
            path,
        )?;
        ensure_within_limit(
            self.limits,
            ContractLimitMetric::JsonDepth,
            self.limits.max_nesting_depth(),
            depth,
            path,
        )
    }

    fn child_depth<E>(&mut self, depth: u64, path: &str) -> Result<u64, E>
    where
        E: de::Error,
    {
        depth.checked_add(1).ok_or_else(|| {
            self.record_error::<E>(limit_exceeded(
                self.limits,
                ContractLimitMetric::JsonDepth,
                self.limits.max_nesting_depth(),
                u64::MAX,
                path,
            ))
        })
    }

    fn record_error<E>(&mut self, error: ContractError) -> E
    where
        E: de::Error,
    {
        self.limit_error = Some(error);
        E::custom("contract JSON admission limit exceeded")
    }
}

struct BoundedValueSeed<'a> {
    state: &'a mut AdmissionState,
    depth: u64,
    path: String,
}

impl<'de> DeserializeSeed<'de> for BoundedValueSeed<'_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if let Err(error) = self.state.observe_value(self.depth, &self.path) {
            return Err(self.state.record_error(error));
        }
        deserializer.deserialize_any(BoundedValueVisitor {
            state: self.state,
            depth: self.depth,
            path: self.path,
        })
    }
}

struct BoundedValueVisitor<'a> {
    state: &'a mut AdmissionState,
    depth: u64,
    path: String,
}

impl<'de> Visitor<'de> for BoundedValueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value within the configured contract limits")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON numbers must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_string()))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        loop {
            let child_path = format!("{}[{}]", self.path, values.len());
            let child_depth = self.state.child_depth(self.depth, &child_path)?;
            let Some(value) = sequence.next_element_seed(BoundedValueSeed {
                state: self.state,
                depth: child_depth,
                path: child_path,
            })?
            else {
                break;
            };
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            let child_path = object_child_path(&self.path, &key);
            let child_depth = self.state.child_depth(self.depth, &child_path)?;
            let value = object.next_value_seed(BoundedValueSeed {
                state: self.state,
                depth: child_depth,
                path: child_path,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

fn object_child_path(parent: &str, key: &str) -> String {
    let mut chars = key.chars();
    let identifier = matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if identifier {
        format!("{parent}.{key}")
    } else {
        let key = serde_json::to_string(key).unwrap_or_else(|_| "\"<invalid-key>\"".to_string());
        format!("{parent}[{key}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_add_reports_a_limit_error_instead_of_wrapping() {
        let limits = ContractLimits::v1();
        assert_eq!(
            checked_observed_add(
                limits,
                ContractLimitMetric::JsonValues,
                limits.max_json_values(),
                u64::MAX,
                1,
                "$.overflow",
            ),
            Err(ContractError::LimitExceeded {
                profile: ContractLimitsProfile::V1,
                metric: ContractLimitMetric::JsonValues,
                limit: limits.max_json_values(),
                observed: u64::MAX,
                path: "$.overflow".to_string(),
            })
        );
    }
}
