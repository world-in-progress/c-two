//! Single-byte shutdown initiation and acknowledgement signals.

use crate::msg_type::{SHUTDOWN_ACK_BYTES, SHUTDOWN_CLIENT_BYTES};

pub fn encode_shutdown_ack() -> [u8; 1] {
    SHUTDOWN_ACK_BYTES
}

pub fn decode_shutdown_ack(payload: &[u8]) -> Result<(), String> {
    if payload != SHUTDOWN_ACK_BYTES {
        return Err("shutdown ack must be exactly one ShutdownAck byte".into());
    }
    Ok(())
}

pub fn encode_shutdown_initiate() -> [u8; 1] {
    SHUTDOWN_CLIENT_BYTES
}

pub fn decode_shutdown_initiate(payload: &[u8]) -> Result<(), String> {
    if payload != SHUTDOWN_CLIENT_BYTES {
        return Err("shutdown initiate must be exactly one ShutdownClient byte".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_signals_are_single_byte_commands() {
        assert_eq!(encode_shutdown_ack(), [0x07]);
        assert_eq!(encode_shutdown_initiate(), [0x05]);
        decode_shutdown_ack(&[0x07]).unwrap();
        decode_shutdown_initiate(&[0x05]).unwrap();
        for payload in [&[][..], &[0x05], &[0x07, b'{', b'}']] {
            assert!(decode_shutdown_ack(payload).is_err());
        }
        for payload in [&[][..], &[0x07], &[0x05, 0]] {
            assert!(decode_shutdown_initiate(payload).is_err());
        }
    }
}
