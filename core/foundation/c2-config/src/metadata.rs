//! Validated metadata carried by a logical call.

use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::sync::Arc;

pub const MAX_CALL_METADATA_ENTRIES: usize = 64;
pub const MAX_CALL_METADATA_KEY_LEN: usize = 64;
pub const MAX_CALL_METADATA_VALUE_LEN: usize = 4096;
pub const MAX_CALL_METADATA_TOTAL_BYTES: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallMetadataError {
    InvalidKeyLength,
    InvalidKeyCharacter,
    InvalidValueLength,
    InvalidValueCharacter,
    ValueBoundarySpace,
    DuplicateKey,
    TooManyEntries,
    TooManyBytes,
}

impl fmt::Display for CallMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKeyLength => write!(f, "key must contain 1 to {MAX_CALL_METADATA_KEY_LEN} bytes"),
            Self::InvalidKeyCharacter => f.write_str("key must contain only ASCII lowercase letters, digits, underscores, periods or hyphens"),
            Self::InvalidValueLength => write!(f, "value exceeds {MAX_CALL_METADATA_VALUE_LEN} bytes"),
            Self::InvalidValueCharacter => f.write_str("value must contain only printable ASCII"),
            Self::ValueBoundarySpace => f.write_str("value must not start or end with a space"),
            Self::DuplicateKey => f.write_str("duplicate key"),
            Self::TooManyEntries => write!(f, "metadata exceeds {MAX_CALL_METADATA_ENTRIES} entries"),
            Self::TooManyBytes => write!(f, "metadata exceeds {MAX_CALL_METADATA_TOTAL_BYTES} bytes"),
        }
    }
}

impl std::error::Error for CallMetadataError {}

pub fn validate_call_metadata_key(key: &str) -> Result<(), CallMetadataError> {
    if key.is_empty() || key.len() > MAX_CALL_METADATA_KEY_LEN {
        return Err(CallMetadataError::InvalidKeyLength);
    }
    if !key
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
    {
        return Err(CallMetadataError::InvalidKeyCharacter);
    }
    Ok(())
}

pub fn validate_call_metadata_value(value: &str) -> Result<(), CallMetadataError> {
    if value.len() > MAX_CALL_METADATA_VALUE_LEN {
        return Err(CallMetadataError::InvalidValueLength);
    }
    if !value.bytes().all(|b| matches!(b, b' '..=b'~')) {
        return Err(CallMetadataError::InvalidValueCharacter);
    }
    if value.starts_with(' ') || value.ends_with(' ') {
        return Err(CallMetadataError::ValueBoundarySpace);
    }
    Ok(())
}

/// An immutable, sorted map shared by clones. An empty map needs no allocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct CallMetadata(Option<Arc<BTreeMap<String, String>>>);

impl CallMetadata {
    pub const fn empty() -> Self {
        Self(None)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    pub fn len(&self) -> usize {
        self.0.as_deref().map_or(0, BTreeMap::len)
    }

    pub fn total_bytes(&self) -> usize {
        self.iter()
            .map(|(key, value)| key.len() + value.len())
            .sum()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.as_deref()?.get(key).map(String::as_str)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .flat_map(|map| map.iter())
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn from_entries<I, K, V>(entries: I) -> Result<Self, CallMetadataError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        collect_entries(entries).map(Self::from_validated_map)
    }

    pub fn from_btree(map: BTreeMap<String, String>) -> Result<Self, CallMetadataError> {
        validate_entry_count(map.len())?;
        let mut total = 0;
        for (key, value) in &map {
            validate_call_metadata_key(key)?;
            validate_call_metadata_value(value)?;
            total += key.len() + value.len();
            validate_total_bytes(total)?;
        }
        Ok(Self::from_validated_map(map))
    }

    /// Inserts a new entry or replaces the value of an existing key.
    pub fn with_entry(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, CallMetadataError> {
        let key = key.into();
        let value = value.into();
        validate_call_metadata_key(&key)?;
        validate_call_metadata_value(&value)?;
        let existing = self.get(&key);
        validate_entry_count(self.len() + usize::from(existing.is_none()))?;
        let replaced_bytes = existing.map_or(0, |previous| key.len() + previous.len());
        validate_total_bytes(self.total_bytes() - replaced_bytes + key.len() + value.len())?;
        let mut map = self.0.as_deref().cloned().unwrap_or_default();
        map.insert(key, value);
        Ok(Self::from_validated_map(map))
    }

    /// Merges entries over the current map, rejecting duplicates within the input.
    pub fn extend<I, K, V>(&self, entries: I) -> Result<Self, CallMetadataError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let additions = collect_entries(entries)?;
        if additions.is_empty() {
            return Ok(self.clone());
        }
        let Some(original) = self.0.as_deref() else {
            return Ok(Self::from_validated_map(additions));
        };
        let mut map = original.clone();
        map.extend(additions);
        validate_entry_count(map.len())?;
        validate_total_bytes(map.iter().map(|(key, value)| key.len() + value.len()).sum())?;
        Ok(Self::from_validated_map(map))
    }

    pub fn without(&self, key: &str) -> Self {
        let Some(original) = self.0.as_deref().filter(|map| map.contains_key(key)) else {
            return self.clone();
        };
        if original.len() == 1 {
            return Self::empty();
        }
        let mut map = original.clone();
        map.remove(key);
        Self::from_validated_map(map)
    }

    fn from_validated_map(map: BTreeMap<String, String>) -> Self {
        Self((!map.is_empty()).then(|| Arc::new(map)))
    }
}

fn validate_entry_count(len: usize) -> Result<(), CallMetadataError> {
    if len > MAX_CALL_METADATA_ENTRIES {
        return Err(CallMetadataError::TooManyEntries);
    }
    Ok(())
}

fn validate_total_bytes(total: usize) -> Result<(), CallMetadataError> {
    if total > MAX_CALL_METADATA_TOTAL_BYTES {
        return Err(CallMetadataError::TooManyBytes);
    }
    Ok(())
}

fn collect_entries<I, K, V>(entries: I) -> Result<BTreeMap<String, String>, CallMetadataError>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let mut map = BTreeMap::new();
    let mut total = 0;
    for (key, value) in entries {
        let key = key.into();
        let value = value.into();
        validate_call_metadata_key(&key)?;
        validate_call_metadata_value(&value)?;
        let new_len = map.len() + 1;
        let entry_bytes = key.len() + value.len();
        match map.entry(key) {
            Entry::Vacant(entry) => {
                validate_entry_count(new_len)?;
                total += entry_bytes;
                validate_total_bytes(total)?;
                entry.insert(value);
            }
            Entry::Occupied(_) => return Err(CallMetadataError::DuplicateKey),
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metadata_is_canonical() {
        assert_eq!(
            std::mem::size_of::<CallMetadata>(),
            std::mem::size_of::<usize>()
        );
        assert!(CallMetadata::default().0.is_none());
        assert!(
            CallMetadata::from_btree(BTreeMap::new())
                .unwrap()
                .0
                .is_none()
        );
        let metadata = CallMetadata::empty().with_entry("z", "").unwrap();
        let extended = metadata.extend(std::iter::empty::<(&str, &str)>()).unwrap();
        assert!(Arc::ptr_eq(
            metadata.0.as_ref().unwrap(),
            extended.0.as_ref().unwrap()
        ));
        assert!(metadata.without("z").0.is_none());
        assert_eq!(CallMetadata::empty().iter().next(), None);
    }

    #[test]
    fn merge_enforces_limits_on_the_final_mapping() {
        let entries = (0..MAX_CALL_METADATA_ENTRIES).map(|i| (format!("k{i:02}"), "v".repeat(253)));
        let original = CallMetadata::from_entries(entries).unwrap();
        assert_eq!(original.total_bytes(), MAX_CALL_METADATA_TOTAL_BYTES);
        let replaced = original.with_entry("k63", "").unwrap();
        assert_eq!(replaced.len(), MAX_CALL_METADATA_ENTRIES);
        assert_eq!(replaced.get("k63"), Some(""));
        assert!(replaced.contains_key("k63"));
        assert_eq!(
            replaced.with_entry("new", ""),
            Err(CallMetadataError::TooManyEntries)
        );
        assert_eq!(
            original.with_entry("k63", "v".repeat(254)),
            Err(CallMetadataError::TooManyBytes)
        );

        let merged = original
            .extend([("k63", "v".repeat(254)), ("k00", "v".repeat(252))])
            .unwrap();
        assert_eq!(merged.total_bytes(), MAX_CALL_METADATA_TOTAL_BYTES);
        assert_eq!(merged.iter().next().unwrap().0, "k00");
        assert_eq!(merged.iter().last().unwrap().0, "k63");
        assert_eq!(
            original.extend([("k00", "one"), ("k00", "two")]),
            Err(CallMetadataError::DuplicateKey)
        );
        assert_eq!(
            CallMetadata::from_entries([("z", "one"), ("z", "two")]),
            Err(CallMetadataError::DuplicateKey)
        );
    }

    #[test]
    fn keys_and_values_follow_the_ascii_and_length_rules() {
        for invalid in ["", "A", "a b", "a/b", "a\t", "é", &"a".repeat(65)] {
            assert!(validate_call_metadata_key(invalid).is_err(), "{invalid:?}");
        }
        assert!(validate_call_metadata_key(&"a-0.".repeat(16)).is_ok());
        assert!(validate_call_metadata_key("trace_id").is_ok());
        for invalid in [
            " value",
            "value ",
            " ",
            "a\tb",
            "a\rb",
            "a\nb",
            "\x7f",
            "é",
            &"x".repeat(4097),
        ] {
            assert!(
                validate_call_metadata_value(invalid).is_err(),
                "{invalid:?}"
            );
        }
        for valid in ["", "a b", "~", &"x".repeat(4096)] {
            assert!(validate_call_metadata_value(valid).is_ok(), "{valid:?}");
        }
        assert_eq!(
            CallMetadata::from_btree(BTreeMap::from([("Key".into(), "value".into())])),
            Err(CallMetadataError::InvalidKeyCharacter)
        );
    }
}
