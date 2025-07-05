use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use futures_util::StreamExt;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::{AppState, Stats, TimestampedStats};

pub async fn container_stats_handler(app: Arc<AppState>) -> anyhow::Result<()> {
    let ws_url = std::env::var("STATS_WS_URL")
        .unwrap_or_else(|_| "ws://localhost:3000/stats/ws".to_string());

    let url =
        Url::parse(&ws_url).with_context(|| format!("Invalid WebSocket URL: '{}'", ws_url))?;

    loop {
        tracing::debug!("[WS] Connecting to stats stream...");

        let connection = connect_async(url.clone()).await;

        let (mut ws_stream, _) = match connection {
            Ok(conn) => {
                tracing::debug!("[WS] Connected.");
                conn
            }
            Err(e) => {
                tracing::debug!("[WS] Connection failed: {}", e);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    match serde_json::from_str::<BTreeMap<String, Stats>>(&text) {
                        Ok(parsed_stats) => {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();

                            let mut stats_guard = app.stats.write().await;

                            for (id, new_stat) in parsed_stats {
                                stats_guard
                                    .entry(id.clone())
                                    .and_modify(|existing| {
                                        if now > existing.timestamp {
                                            *existing = TimestampedStats {
                                                stats: new_stat.clone(),
                                                timestamp: now,
                                            };
                                        }
                                    })
                                    .or_insert(TimestampedStats {
                                        stats: new_stat,
                                        timestamp: now,
                                    });
                            }

                            drop(stats_guard);

                            push_stats_to_ws_clients(app.clone()).await;
                        }
                        Err(e) => {
                            tracing::debug!("[WS] JSON parse error: {}", e);
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    tracing::debug!("[WS] Connection closed by peer. Reconnecting...");
                    break;
                }
                Ok(_) => {} // Binary, Ping, Pong, etc. — ignore for now
                Err(e) => {
                    tracing::debug!("[WS] Error: {} — reconnecting...", e);
                    break;
                }
            }
        }

        // Wait a bit before retrying
        sleep(Duration::from_secs(2)).await;
    }
}

pub async fn push_stats_to_ws_clients(app: Arc<AppState>) {
    let stats = app.stats.read().await;

    match serde_json::to_value(&stats.clone()) {
        Ok(serialized) => {
            let _ = app.stats_tx.send(serialized);
        }
        Err(e) => {
            tracing::warn!("Failed to serialize stats: {}", e);
        }
    }
}
