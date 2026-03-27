//! Axum router for the relay server.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};

use crate::state::RelayState;

/// Build the relay axum router.
pub fn build_router(state: RelayState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/routes", get(routes_handler))
        .route("/{route_name}/{method_name}", post(call_handler))
        .with_state(state)
}

// ── Handlers ─────────────────────────────────────────────────────────────

/// `GET /health` — liveness check.
async fn health(State(state): State<RelayState>) -> impl IntoResponse {
    let client = state.client.read().await;
    let route_names = client.route_names();
    let body = serde_json::json!({
        "status": "ok",
        "routes": route_names,
    });
    axum::Json(body)
}

/// `GET /routes` — list routes and methods.
async fn routes_handler(State(state): State<RelayState>) -> impl IntoResponse {
    let client = state.client.read().await;
    let route_names = client.route_names();
    let routes: Vec<serde_json::Value> = route_names
        .iter()
        .filter_map(|name| {
            client.route_table(name).map(|table| {
                serde_json::json!({
                    "name": name,
                    "methods": table.method_names(),
                })
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "routes": routes }))
}

/// `POST /{route_name}/{method_name}` — relay CRM call.
async fn call_handler(
    State(state): State<RelayState>,
    Path((route_name, method_name)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let client = state.client.read().await;
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
