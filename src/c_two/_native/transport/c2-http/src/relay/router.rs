//! Axum router for the multi-upstream relay server.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use c2_ipc::IpcClient;
use crate::relay::state::RelayState;
use crate::relay::peer::{PeerEnvelope, PeerMessage};
use crate::relay::peer_handlers;

/// Build the relay axum router with control-plane and data-plane endpoints.
pub fn build_router(state: Arc<RelayState>) -> Router {
    Router::new()
        .route("/_register", post(handle_register))
        .route("/_unregister", post(handle_unregister))
        .route("/_routes", get(handle_list_routes))
        .route("/_resolve/{name}", get(handle_resolve))
        .route("/_peers", get(handle_peers))
        .route("/_peer/announce", post(peer_handlers::handle_peer_announce))
        .route("/_peer/join", post(peer_handlers::handle_peer_join))
        .route("/_peer/sync", get(peer_handlers::handle_peer_sync))
        .route("/_peer/heartbeat", post(peer_handlers::handle_peer_heartbeat))
        .route("/_peer/leave", post(peer_handlers::handle_peer_leave))
        .route("/_peer/digest", post(peer_handlers::handle_peer_digest))
        .route("/health", get(handle_health))
        .route("/_echo", post(echo_handler))
        .route("/{route_name}/{method_name}", post(call_handler))
        .with_state(state)
        .layer(DefaultBodyLimit::disable())
}

// -- Control-plane handlers -----------------------------------------------

/// `POST /_register` — register a new upstream CRM.
///
/// Body: `{"name": "grid", "address": "ipc://...", "crm_ns": "...", "crm_ver": "..."}`
/// Returns: 201 on success, 409 on duplicate, 502 on connection failure.
async fn handle_register(
    State(state): State<Arc<RelayState>>,
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
    let crm_ns = body.get("crm_ns").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let crm_ver = body.get("crm_ver").and_then(|v| v.as_str()).unwrap_or("").to_string();

    // Check for duplicate LOCAL route.
    if state.has_local_route(&name) {
        let existing_addr = state.get_address(&name);
        if let Some(ref old_addr) = existing_addr {
            if old_addr != &address {
                // Different address → might be a new process after crash.
                // Check if old IPC connection is still alive.
                if let Some(old_client) = state.get_client(&name) {
                    if old_client.is_connected() {
                        // Old process still alive → reject.
                        return (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({
                                "error": "DuplicateRoute",
                                "name": name,
                                "existing_address": old_addr,
                            })),
                        ).into_response();
                    }
                    // Not connected → fall through to upsert.
                }
            }
            // Same address or old process dead → fall through to upsert.
        }
    }

    // Connect IPC client (or skip if configured for testing)
    let client = if state.config().skip_ipc_validation {
        Arc::new(IpcClient::new(&address))
    } else {
        let mut c = IpcClient::new(&address);
        if let Err(e) = c.connect().await {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Failed to connect upstream '{name}' at {address}: {e}")})),
            ).into_response();
        }
        Arc::new(c)
    };

    let entry = state.register_upstream(name.clone(), address, crm_ns, crm_ver, client);

    // Gossip announce to peers
    let envelope = PeerEnvelope::new(
        state.relay_id(),
        PeerMessage::RouteAnnounce {
            name: entry.name.clone(),
            relay_id: entry.relay_id.clone(),
            relay_url: entry.relay_url.clone(),
            ipc_address: entry.ipc_address.clone(),
            crm_ns: entry.crm_ns.clone(),
            crm_ver: entry.crm_ver.clone(),
            registered_at: entry.registered_at,
        },
    );
    let peers = state.list_peers();
    state.disseminator().broadcast(envelope, &peers);

    (StatusCode::CREATED, Json(serde_json::json!({"registered": name}))).into_response()
}

/// `POST /_unregister` — remove a CRM upstream.
///
/// Body: `{"name": "grid"}`
/// Returns: 200 on success, 404 on missing.
async fn handle_unregister(
    State(state): State<Arc<RelayState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing \"name\"").into_response(),
    };

    match state.unregister_upstream(&name) {
        Some((entry, old_client)) => {
            // Close old client asynchronously
            if let Some(arc_client) = old_client {
                tokio::spawn(async move {
                    let mut client = match Arc::try_unwrap(arc_client) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    client.close().await;
                });
            }

            // Gossip withdraw to peers
            let envelope = PeerEnvelope::new(
                state.relay_id(),
                PeerMessage::RouteWithdraw {
                    name: entry.name.clone(),
                    relay_id: entry.relay_id.clone(),
                },
            );
            let peers = state.list_peers();
            state.disseminator().broadcast(envelope, &peers);

            (StatusCode::OK, Json(serde_json::json!({"unregistered": name}))).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Route name not registered: '{name}'")})),
        ).into_response(),
    }
}

/// `GET /_routes` — list all registered routes.
async fn handle_list_routes(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    let routes: Vec<serde_json::Value> = state.list_routes()
        .into_iter()
        .map(|r| serde_json::json!({"name": r.name, "address": r.ipc_address.unwrap_or_default()}))
        .collect();
    Json(serde_json::json!({"routes": routes}))
}

// -- Data-plane handlers --------------------------------------------------

/// `GET /health` — liveness check.
async fn handle_health(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    let route_names = state.route_names();
    Json(serde_json::json!({
        "status": "ok",
        "routes": route_names,
    }))
}

/// `GET /_resolve/{name}` — resolve a CRM name to available routes.
async fn handle_resolve(
    Path(name): Path<String>,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    let routes = state.resolve(&name);
    if routes.is_empty() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "ResourceNotFound", "name": name,
        }))).into_response();
    }
    Json(routes).into_response()
}

/// `GET /_peers` — list known peer relays.
async fn handle_peers(
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    Json(state.list_peers()).into_response()
}

/// `POST /{route_name}/{method_name}` — relay CRM call to upstream.
///
/// If the upstream was evicted by the idle sweeper, attempts a lazy
/// reconnect before returning 502.
async fn call_handler(
    State(state): State<Arc<RelayState>>,
    Path((route_name, method_name)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let client = state.get_client(&route_name);

    let client = match client {
        Some(c) => c,
        None => match try_reconnect(&state, &route_name).await {
            Some(c) => c,
            None => {
                let has = state.has_connection(&route_name);
                if has {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({
                            "error": format!("Upstream '{route_name}' is registered but unreachable")
                        })),
                    ).into_response();
                }
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": format!("No upstream registered for route: '{route_name}'")
                    })),
                ).into_response();
            }
        },
    };

    match client.call(&route_name, &method_name, &body).await {
        Ok(result) => {
            state.touch_connection(&route_name);
            let bytes = result.into_bytes_with_pool(
                client.server_pool_arc(),
                &client.reassembly_pool_arc(),
            ).unwrap_or_default();
            (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            ).into_response()
        }
        Err(c2_ipc::IpcError::CrmError(err_bytes)) => {
            state.touch_connection(&route_name);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "application/octet-stream")],
                err_bytes,
            ).into_response()
        }
        Err(e) => {
            // Evict dead client so next request triggers reconnect.
            state.evict_connection(&route_name);
            (
                StatusCode::BAD_GATEWAY,
                [("content-type", "text/plain")],
                format!("relay error: {e}"),
            ).into_response()
        }
    }
}

/// Attempt to reconnect an evicted upstream.
async fn try_reconnect(state: &RelayState, route_name: &str) -> Option<Arc<IpcClient>> {
    let address = state.get_address(route_name)?;

    let mut client = IpcClient::new(&address);
    match client.connect().await {
        Ok(()) => {
            let client = Arc::new(client);
            state.reconnect(route_name, client.clone());
            Some(client)
        }
        Err(e) => {
            eprintln!("[relay] Failed to reconnect upstream '{route_name}': {e}");
            None
        }
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
