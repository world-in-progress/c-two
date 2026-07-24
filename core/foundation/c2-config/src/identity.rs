pub fn validate_ipc_region_id(region: &str) -> Result<(), String> {
    validate_id("region ID", region)
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
    fn ipc_region_id_rejects_path_like_values() {
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
                validate_ipc_region_id(value).is_err(),
                "region should be rejected: {value:?}"
            );
        }
    }

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
        validate_ipc_region_id("unit-server").unwrap();
        validate_server_id("unit-server").unwrap();
    }
}
