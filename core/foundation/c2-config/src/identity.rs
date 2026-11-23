pub const MAX_CALL_ID_LEN: usize = 128;

pub fn validate_call_id(call_id: &str) -> Result<(), String> {
    if call_id.is_empty() {
        return Err("call_id cannot be empty".to_string());
    }
    if call_id.len() > MAX_CALL_ID_LEN {
        return Err(format!("call_id cannot exceed {MAX_CALL_ID_LEN} bytes"));
    }
    if !call_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("call_id must contain only ASCII [A-Za-z0-9_-]".to_string());
    }
    Ok(())
}

pub fn validate_server_id(server_id: &str) -> Result<(), String> {
    validate_id("server_id", server_id)
}

pub fn validate_relay_id(relay_id: &str) -> Result<(), String> {
    validate_id("relay_id", relay_id)?;
    if relay_id.len() > 255 {
        return Err("relay_id cannot exceed 255 bytes".to_string());
    }
    Ok(())
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if value.trim() != value {
        return Err(format!(
            "{label} cannot contain leading or trailing whitespace"
        ));
    }
    if value == "." || value == ".." || value.contains('/') || value.contains('\\') {
        return Err(format!("{label} cannot contain path separators"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} cannot contain control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_id_rejects_path_like_values() {
        for value in [
            "",
            "../escape",
            "bad/name",
            "bad\\name",
            ".",
            "..",
            " leading",
            "trailing ",
            "bad\nname",
        ] {
            assert!(
                validate_server_id(value).is_err(),
                "server_id should be rejected: {value:?}"
            );
        }
    }

    #[test]
    fn relay_id_rejects_path_like_or_control_values() {
        for value in [
            "",
            "../escape",
            "bad/name",
            "bad\\name",
            ".",
            "..",
            " leading",
            "trailing ",
            "bad\nrelay",
        ] {
            assert!(
                validate_relay_id(value).is_err(),
                "relay_id should be rejected: {value:?}"
            );
        }
        let too_long = "x".repeat(256);
        assert!(validate_relay_id(&too_long).is_err());
    }

    #[test]
    fn relay_id_accepts_generated_style_values() {
        validate_relay_id("host_1234_abcd1234").unwrap();
        validate_relay_id("relay-a").unwrap();
    }

    #[test]
    fn plain_ids_are_accepted() {
        validate_server_id("unit-server").unwrap();
    }

    #[test]
    fn call_id_validation_enforces_ascii_charset_and_bounds() {
        validate_call_id("a").unwrap();
        validate_call_id("step-001_AZaz09").unwrap();
        validate_call_id(&"a".repeat(MAX_CALL_ID_LEN)).unwrap();

        for bad in [
            "",
            "has space",
            "has/slash",
            "has.dot",
            "..",
            "bad:colon",
            "non_ascii_中",
            "lead\n",
        ] {
            assert!(validate_call_id(bad).is_err(), "expected rejection: {bad:?}");
        }
        assert!(validate_call_id(&"a".repeat(MAX_CALL_ID_LEN + 1)).is_err());
    }
}
