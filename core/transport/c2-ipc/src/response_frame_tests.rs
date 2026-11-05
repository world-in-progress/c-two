//! An inline response moves the admitted frame allocation into its carrier.

use super::take_inline_response;

#[test]
fn inline_carrier_keeps_the_original_allocation_without_a_whole_value_clone() {
    let mut frame = Vec::with_capacity(64 * 1024);
    frame.extend_from_slice(&[0, 1]);
    frame.extend((0..4096).map(|n| (n % 251) as u8));
    let pointer = frame.as_ptr();
    let capacity = frame.capacity();
    let response = take_inline_response(&mut frame, 4096);
    assert_eq!(response.as_ptr(), pointer);
    assert_eq!(response.capacity(), capacity);
    assert_eq!(response.len(), 4096);
    assert_eq!(response[0], 0);
    assert_eq!(response[4095], (4095 % 251) as u8);
    assert_eq!(frame.capacity(), 0);
}
