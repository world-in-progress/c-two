use std::convert::Infallible;

use bytes::Bytes;
use futures::stream;

use crate::client::{HttpError, HttpInputOwner};

// Local response violations use the same canonical semantic envelope as
// remote HTTP errors. Preserve the actual status, including 200/500; Core
// normalizes the envelope on both call and probe/resolution paths. The error
// code is ProtocolViolation, never stale-route authority or POST replay proof.
fn response_protocol_error(
    reason: &str,
    max_payload_size: u64,
    observed: String,
) -> c2_error::C2Error {
    c2_error::C2Error::new(
        c2_error::ErrorCode::ProtocolViolation,
        "HTTP response violates the selected route's message boundary",
    )
    .with_details(std::collections::BTreeMap::from([
        ("transport".into(), "http".into()),
        ("reason".into(), reason.into()),
        ("max_payload_size".into(), max_payload_size.to_string()),
        ("observed_payload_size".into(), observed),
    ]))
}

fn checked_response_length(
    current: usize,
    incoming: usize,
    max_payload_size: u64,
) -> Result<usize, c2_error::C2Error> {
    let length = current.checked_add(incoming).ok_or_else(|| {
        response_protocol_error(
            "response_payload_length_overflow",
            max_payload_size,
            "overflow".into(),
        )
    })?;
    let wire_length = u64::try_from(length).map_err(|_| {
        response_protocol_error(
            "response_payload_length_overflow",
            max_payload_size,
            length.to_string(),
        )
    })?;
    if wire_length > max_payload_size {
        return Err(response_protocol_error(
            "response_payload_too_large",
            max_payload_size,
            wire_length.to_string(),
        ));
    }
    Ok(length)
}

// Called only after required length has passed the message-boundary check.
// Grow from existing capacity, never from Content-Length or the whole limit.
fn response_growth_capacity(current: usize, required: usize, max_payload_size: u64) -> usize {
    let ceiling = usize::try_from(max_payload_size).unwrap_or(usize::MAX);
    current
        .checked_mul(2)
        .unwrap_or(ceiling)
        .max(required)
        .min(ceiling)
}

fn append_response_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    max_payload_size: u64,
) -> Result<(), c2_error::C2Error> {
    let length = checked_response_length(body.len(), chunk.len(), max_payload_size)?;
    if length > body.capacity() {
        let capacity = response_growth_capacity(body.capacity(), length, max_payload_size);
        // Request the capped geometric target exactly; Vec's own growth policy
        // must not add another doubling beyond the native message ceiling.
        body.try_reserve_exact(capacity - body.len()).map_err(|_| {
            response_protocol_error(
                "response_payload_allocation_failed",
                max_payload_size,
                length.to_string(),
            )
        })?;
    }
    body.extend_from_slice(chunk);
    Ok(())
}

pub(crate) async fn read_http_response_body(
    mut response: reqwest::Response,
    max_payload_size: u64,
) -> Result<Vec<u8>, HttpError> {
    let status = response.status().as_u16();
    let protocol_error = |error: c2_error::C2Error| {
        HttpError::ServerError(
            status,
            serde_json::to_string(&error.envelope())
                .expect("canonical error envelope is serializable"),
        )
    };
    // reqwest's content_length is the *body* size hint, not the raw header.
    // HEAD/304 metadata (and decoded bodies) can legitimately differ from the
    // header. Never allocate from either hint or the entire configured limit.
    if let Some(length) = response.content_length()
        && length > max_payload_size
    {
        return Err(protocol_error(response_protocol_error(
            "response_payload_too_large",
            max_payload_size,
            length.to_string(),
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| HttpError::Transport(error.to_string()))?
    {
        append_response_chunk(&mut body, &chunk, max_payload_size).map_err(&protocol_error)?;
    }
    Ok(body)
}

// One response reader for every status and both owned/borrowed call paths.
pub(crate) async fn read_http_response(
    response: reqwest::Response,
    max_payload_size: u64,
) -> Result<Vec<u8>, HttpError> {
    let status = response.status().as_u16();
    let body = read_http_response_body(response, max_payload_size).await?;
    match status {
        200 => Ok(body),
        500 => Err(HttpError::CrmError(body)),
        code => Err(HttpError::ServerError(
            code,
            String::from_utf8_lossy(&body).into_owned(),
        )),
    }
}

pub(crate) fn validate_remote_payload_chunk_size(chunk_size: u64) -> Result<usize, HttpError> {
    c2_config::validate_remote_payload_chunk_size(chunk_size)
        .map_err(|reason| HttpError::InvalidInput(format!("remote_payload_chunk_size {reason}")))?;
    usize::try_from(chunk_size).map_err(|_| {
        HttpError::InvalidInput(format!(
            "remote_payload_chunk_size {chunk_size} exceeds this platform's addressable memory"
        ))
    })
}

#[cfg(test)]
pub(crate) fn payload_chunk_ranges(
    len: usize,
    chunk_size: usize,
) -> impl Iterator<Item = (usize, usize)> {
    (0..len).step_by(chunk_size).map(move |start| {
        let end = start.saturating_add(chunk_size).min(len);
        (start, end)
    })
}

pub(crate) fn reqwest_body_from_payload(
    data: &[u8],
    chunk_size: u64,
) -> Result<reqwest::Body, HttpError> {
    let chunk_size = validate_remote_payload_chunk_size(chunk_size)?;
    if data.len() <= chunk_size {
        return Ok(reqwest::Body::from(data.to_vec()));
    }
    Ok(reqwest::Body::wrap_stream(payload_chunks(
        Bytes::copy_from_slice(data),
        chunk_size,
    )))
}

// Keep the caller's complete owner (bytes plus any retention permit) behind
// Bytes so each streamed slice shares its original allocation and lifetime.
struct OwnedPayload(HttpInputOwner);

impl AsRef<[u8]> for OwnedPayload {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref().as_ref()
    }
}

pub(crate) fn reqwest_body_from_owned_payload(
    data: HttpInputOwner,
    chunk_size: u64,
) -> Result<reqwest::Body, HttpError> {
    let chunk_size = validate_remote_payload_chunk_size(chunk_size)?;
    let bytes = Bytes::from_owner(OwnedPayload(data));
    if bytes.len() <= chunk_size {
        return Ok(reqwest::Body::from(bytes));
    }
    Ok(reqwest::Body::wrap_stream(payload_chunks(
        bytes, chunk_size,
    )))
}

fn payload_chunks(
    bytes: Bytes,
    chunk_size: usize,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> + Send {
    let len = bytes.len();
    stream::unfold((bytes, 0usize), move |(bytes, offset)| async move {
        if offset >= len {
            return None;
        }
        let end = offset.saturating_add(chunk_size).min(len);
        let chunk = bytes.slice(offset..end);
        Some((Ok::<Bytes, Infallible>(chunk), (bytes, end)))
    })
}

#[cfg(feature = "relay")]
pub(crate) fn axum_body_from_payload(
    data: Vec<u8>,
    chunk_size: u64,
) -> Result<axum::body::Body, HttpError> {
    let chunk_size = validate_remote_payload_chunk_size(chunk_size)?;
    if data.len() <= chunk_size {
        return Ok(axum::body::Body::from(data));
    }
    let bytes = Bytes::from(data);
    let len = bytes.len();
    let chunks = stream::unfold((bytes, 0usize), move |(bytes, offset)| async move {
        if offset >= len {
            return None;
        }
        let end = offset.saturating_add(chunk_size).min(len);
        let chunk = bytes.slice(offset..end);
        Some((Ok::<Bytes, Infallible>(chunk), (bytes, end)))
    });
    Ok(axum::body::Body::from_stream(chunks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn response_length_checks_exact_limit_and_platform_overflow() {
        assert_eq!(checked_response_length(3, 5, 8).unwrap(), 8);
        assert_eq!(checked_response_length(0, 0, 0).unwrap(), 0);
        for (current, incoming, limit, reason) in [
            (3, 6, 8, "response_payload_too_large"),
            (usize::MAX, 1, u64::MAX, "response_payload_length_overflow"),
        ] {
            let error = checked_response_length(current, incoming, limit).unwrap_err();
            assert_eq!(error.code, c2_error::ErrorCode::ProtocolViolation);
            assert_eq!(error.details["reason"], reason);
        }
    }

    #[test]
    fn tiny_response_chunks_have_bounded_capacity_growth_and_reject_before_reserve() {
        const LIMIT: usize = 8192;
        let mut body = Vec::new();
        let mut growths = 0;
        for index in 0..LIMIT {
            let previous_capacity = body.capacity();
            append_response_chunk(&mut body, &[(index % 251) as u8], LIMIT as u64).unwrap();
            growths += usize::from(body.capacity() != previous_capacity);
            assert!(body.capacity() <= LIMIT);
        }
        assert_eq!(
            body,
            (0..LIMIT)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>()
        );
        let capacity = body.capacity();
        let error = append_response_chunk(&mut body, &[0], LIMIT as u64).unwrap_err();
        assert_eq!(error.code, c2_error::ErrorCode::ProtocolViolation);
        assert_eq!(error.details["reason"], "response_payload_too_large");
        assert_eq!(body.len(), LIMIT);
        assert_eq!(
            body.capacity(),
            capacity,
            "rejection must precede allocation"
        );
        println!("8192 one-byte chunks: {growths} capacity changes");
        assert!(
            growths <= 14,
            "8192 one-byte chunks caused {growths} capacity growths"
        );
    }

    #[test]
    fn response_capacity_growth_clamps_message_and_platform_limits_without_allocation() {
        assert_eq!(response_growth_capacity(0, 0, u64::MAX), 0);
        assert_eq!(response_growth_capacity(0, 1, u64::MAX), 1);
        assert_eq!(response_growth_capacity(4, 5, 7), 7);
        assert_eq!(response_growth_capacity(4, 10, 11), 10);
        let above_half = usize::MAX / 2 + 1;
        assert_eq!(
            response_growth_capacity(above_half, above_half + 1, u64::MAX),
            usize::MAX
        );
    }

    #[test]
    fn oversized_first_response_chunk_does_not_allocate() {
        let mut body = Vec::new();
        let error = append_response_chunk(&mut body, &[0; 9], 8).unwrap_err();
        assert_eq!(error.code, c2_error::ErrorCode::ProtocolViolation);
        assert_eq!(error.details["reason"], "response_payload_too_large");
        assert!(body.is_empty());
        assert_eq!(body.capacity(), 0);
    }

    #[cfg(feature = "relay")]
    #[tokio::test]
    async fn response_reader_accepts_tiny_chunk_stream_without_limit_preallocation() {
        let body = reqwest::Body::wrap_stream(stream::iter(
            (0..8192).map(|_| Ok::<_, Infallible>(Bytes::from_static(b"x"))),
        ));
        let response: reqwest::Response = axum::http::Response::new(body).into();
        assert_eq!(response.content_length(), None);
        let body = read_http_response_body(response, u64::MAX).await.unwrap();
        assert_eq!(body.len(), 8192);
        assert!(body.iter().all(|byte| *byte == b'x'));
        assert_eq!(body.capacity(), 8192);
    }

    struct CountedInput {
        bytes: Vec<u8>,
        drops: Arc<AtomicUsize>,
    }

    impl AsRef<[u8]> for CountedInput {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }

    impl Drop for CountedInput {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn inline_body_retains_custom_owner_after_attempt_future_finishes() {
        for len in [0, 1, 3] {
            let drops = Arc::new(AtomicUsize::new(0));
            let input = Arc::new(CountedInput {
                bytes: vec![42; len],
                drops: Arc::clone(&drops),
            });
            let pointer = input.bytes.as_ptr();
            let weak = Arc::downgrade(&input);
            let owner: HttpInputOwner = input.clone();
            let attempt = async move { reqwest_body_from_owned_payload(owner, 3).unwrap() };
            drop(input);
            let body = attempt.await;
            assert_eq!(body.as_bytes().unwrap().as_ptr(), pointer);
            assert_eq!(body.as_bytes().unwrap(), vec![42; len]);
            assert_eq!(weak.strong_count(), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 0);

            drop(body);
            assert!(weak.upgrade().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[cfg(feature = "relay")]
    #[tokio::test]
    async fn body_slices_retain_custom_owner_until_last_transport_reference_drops() {
        let drops = Arc::new(AtomicUsize::new(0));
        let input = Arc::new(CountedInput {
            bytes: vec![1, 2, 3, 4, 5, 6, 7],
            drops: Arc::clone(&drops),
        });
        let pointer = input.bytes.as_ptr();
        let weak = Arc::downgrade(&input);
        let owner: HttpInputOwner = input.clone();
        // A returned request body may remain in reqwest/hyper after the
        // attempt's task-local ownership and future have both disappeared.
        let attempt = async move { reqwest_body_from_owned_payload(owner, 3).unwrap() };
        drop(input);
        let body = attempt.await;
        assert_eq!(weak.strong_count(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let mut stream = axum::body::Body::new(body).into_data_stream();
        let first = stream.next().await.unwrap().unwrap();
        let first_alias = first.clone();
        let second = stream.next().await.unwrap().unwrap();
        assert_eq!(first.as_ptr(), pointer);
        assert_eq!(second.as_ptr(), pointer.wrapping_add(3));
        assert_eq!(first.as_ref(), &[1, 2, 3]);
        assert_eq!(second.as_ref(), &[4, 5, 6]);
        assert_eq!(weak.strong_count(), 1);

        // Drop the body with its unconsumed final chunk. Emitted slices still
        // retain the complete owner; dropping any earlier alias cannot refund it.
        drop(stream);
        drop(second);
        drop(first);
        assert_eq!(weak.strong_count(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(first_alias.as_ref(), &[1, 2, 3]);

        drop(first_alias);
        assert!(weak.upgrade().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn payload_chunk_ranges_split_by_configured_size() {
        let ranges = payload_chunk_ranges(7, 3).collect::<Vec<_>>();

        assert_eq!(ranges, vec![(0, 3), (3, 6), (6, 7)]);
    }

    #[test]
    fn owned_small_payload_preserves_allocation_and_owner() {
        for len in [0, 1, 3] {
            let data = Arc::new(vec![42; len]);
            let pointer = data.as_ptr();
            let weak = Arc::downgrade(&data);
            let body = reqwest_body_from_owned_payload(data, 3).unwrap();
            let bytes = body.as_bytes().unwrap();

            assert_eq!(bytes, vec![42; len]);
            assert_eq!(bytes.as_ptr(), pointer);
            assert_eq!(weak.strong_count(), 1);
            drop(body);
            assert!(weak.upgrade().is_none());
        }
    }

    #[tokio::test]
    async fn owned_payload_chunks_share_allocation_and_release_after_last_slice() {
        let data = Arc::new(vec![1, 2, 3, 4, 5, 6, 7]);
        let pointer = data.as_ptr();
        let weak = Arc::downgrade(&data);
        let chunks = payload_chunks(Bytes::from_owner(OwnedPayload(data)), 3);
        let chunks = chunks.collect::<Vec<_>>().await;
        let chunks = chunks.into_iter().map(Result::unwrap).collect::<Vec<_>>();

        assert_eq!(chunks.iter().map(Bytes::len).collect::<Vec<_>>(), [3, 3, 1]);
        assert_eq!(chunks[0].as_ptr(), pointer);
        assert_eq!(chunks[1].as_ptr(), pointer.wrapping_add(3));
        assert_eq!(chunks[2].as_ptr(), pointer.wrapping_add(6));
        assert_eq!(chunks.concat(), [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(weak.strong_count(), 1);
        let last = chunks[2].clone();
        drop(chunks);
        assert_eq!(weak.strong_count(), 1);
        drop(last);
        assert!(weak.upgrade().is_none());
    }

    #[cfg(feature = "relay")]
    #[tokio::test]
    async fn owned_reqwest_body_stream_preserves_sizes_content_and_owner() {
        for len in [4, 6, 7] {
            let data = Arc::new((0..len).map(|i| i as u8).collect::<Vec<_>>());
            let pointer = data.as_ptr();
            let weak = Arc::downgrade(&data);
            let body = reqwest_body_from_owned_payload(data, 3).unwrap();
            assert!(body.as_bytes().is_none());
            let chunks = axum::body::Body::new(body)
                .into_data_stream()
                .collect::<Vec<_>>()
                .await;
            let chunks = chunks.into_iter().map(Result::unwrap).collect::<Vec<_>>();

            let expected_sizes = payload_chunk_ranges(len, 3)
                .map(|(start, end)| end - start)
                .collect::<Vec<_>>();
            assert_eq!(
                chunks.iter().map(Bytes::len).collect::<Vec<_>>(),
                expected_sizes
            );
            assert_eq!(
                chunks.concat(),
                (0..len).map(|i| i as u8).collect::<Vec<_>>()
            );
            for (index, chunk) in chunks.iter().enumerate() {
                assert_eq!(chunk.as_ptr(), pointer.wrapping_add(index * 3));
            }
            assert_eq!(weak.strong_count(), 1);
            drop(chunks);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn invalid_owned_payload_chunk_size_releases_owner() {
        let data = Arc::new(vec![42]);
        let weak = Arc::downgrade(&data);

        assert!(reqwest_body_from_owned_payload(data, 0).is_err());
        assert!(weak.upgrade().is_none());
    }
}
