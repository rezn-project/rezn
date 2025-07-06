use std::sync::Arc;

use axum::{
    extract::{ws::Message, State, WebSocketUpgrade},
    response::IntoResponse,
};

use crate::AppState;

#[utoipa::path(
    get,
    path = "/stats/ws",
    description = "Exposes container stats via WS",
    responses(
        (status = 101, description = "WebSocket upgrade initiated")
    ),
    tag = "Streaming"
)]
pub async fn stats_ws_handler(
    State(app): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = app.stats_tx.subscribe();
        while let Ok(ev) = rx.recv().await {
            // Ignore errors if client closed
            let _ = socket.send(Message::Text(ev.to_string().into())).await;
        }
    })
}
