//! Runtime identity helpers shared by language SDKs.

use crate::LifecycleError;

pub fn validate_server_id(server_id: &str) -> Result<(), LifecycleError> {
    c2_config::validate_server_id(server_id).map_err(LifecycleError::InvalidServerId)
}

pub fn ipc_address_for_server_id(server_id: &str) -> String {
    format!("ipc://{server_id}")
}

pub fn auto_server_id() -> String {
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    format!("cc{:x}{uuid}", std::process::id())
}

pub fn auto_server_instance_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
