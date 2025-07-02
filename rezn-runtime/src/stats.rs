use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::{Stats, StatsMap, TimestampedStats};

pub async fn container_stats_handler(stats: &Arc<RwLock<StatsMap>>) {
    let url = Url::parse("ws://localhost:3000/stats/ws").unwrap();
    let (mut ws_stream, _) = connect_async(url)
        .await
        .expect("WebSocket connection failed");

    println!("Connected. Listening for messages...");

    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<BTreeMap<String, Stats>>(&text) {
                    Ok(parsed_stats) => {
                        for (id, new_stat) in parsed_stats {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();

                            let mut stats_guard = stats.write().await;

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
                    }
                    Err(e) => {
                        eprintln!("Failed to parse incoming stats: {}", e);
                    }
                }
            }
            Ok(Message::Binary(bin)) => {
                println!("Binary message: {:?}", bin);
            }
            Ok(Message::Close(_)) => {
                println!("Server closed the connection.");
                break;
            }
            Ok(_) => {
                println!("Other message received.");
            }
            Err(e) => {
                eprintln!("WebSocket error: {}", e);
                break;
            }
        }
    }
}
