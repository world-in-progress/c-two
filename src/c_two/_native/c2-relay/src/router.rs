//! Axum router for the multi-upstream relay server.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use crate::state::RelayState;

/// Build the relay axum router with control-plane and data-plane endpoints.
pub fn build_router(state: RelayState) -> Router {
    Router::new()
        // Control-plane endpoints (underscore prefix avoids CRM name collisions)
        .route("/_register", post(handle_register))
        .route("/_unregister", post(handle_unregister))
        .route("/_routes", get(handle_routes))
        // Data-plane endpoints
        .route("/health", get(health))
        .route("/_echo", post(echo_handler))
        .route("/{route_name}/{method_name}", post(call_handler))
        .with_state(state)
}

// -- Control-plane handlers -----------------------------------------------

/// `POST /_register` — register a new upstream CRM.
///
/// Body: `{"name": "grid", "address": "ipc-v3://..."}`
/// Returns: 201 on success, 409 on duplicate, 502 on connection failure.
async fn handle_register(
    State(state): State<RelayState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"name\"").into_response(),
    };
    let address = match body.get("address").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"address\"").into_response(),
    };

    let mut pool = state.pool.write().await;
    match pool.add(name.clone(), address).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"registered": name})),
        )
            .into_response(),
        Err(e) if e.contains("already registered") => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// `POST /_unregister` — remove a CRM upstream.
///
/// Body: `{"name": "grid"}`
/// Returns: 200 on success, 404 on missing.
async fn handle_unregister(
    State(state): State<RelayState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"name\"").into_response(),
    };

    let mut pool = state.pool.write().await;
    match pool.remove(&name) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"unregistered": name})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// `GET /_routes` — list all registered routes.
async fn handle_routes(State(state): State<RelayState>) -> impl IntoResponse {
    let pool = state.pool.read().await;
    let routes: Vec<serde_json::Value> = pool
        .list_routes()
        .into_iter()
        .map(|r| serde_json::json!({"name": r.name, "address": r.address}))
        .collect();
    Json(serde_json::json!({"routes": routes}))
}

// -- Data-plane handlers --------------------------------------------------

/// `GET /health` — liveness check.
async fn health(State(state): State<RelayState>) -> impl IntoResponse {
    let pool = state.pool.read().await;
    let route_names = pool.route_names();
    Json(serde_json::json!({
        "status": "ok",
        "routes": route_names,
    }))
}

/// `POST /{route_name}/{method_name}` — relay CRM call to upstream.
async fn call_handler(
    State(state): State<RelayState>,
    Path((route_name, method_name)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let pool = state.pool.read().await;
    let client = match pool.get(&route_name) {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("No upstream registered for route: '{route_name}'")
                })),
            )
                .into_response()
        }
    };

    match client.call(&route_name, &method_name, &body).await {
        Ok(result) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            result,
        )
            .into_response(),
        Err(c2_ipc::IpcError::CrmError(err_bytes)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("content-type", "application/octet-stream")],
            err_bytes,
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            [("content-type", "text/plain")],
            format!("relay error: {e}"),
        )
            .into_response(),
    }
}

/// `POST /_echo` — echo endpoint for benchmarking the relay itself.
///
/// Returns the request body immediately with no IPC round-trip.
async fn echo_handler(body: Bytes) -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/octet-stream")],
        body,
    )
        .into_response()
}
