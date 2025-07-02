mod orqos_client;
mod reconcile;

mod router;
mod routes;

use std::env;
use std::sync::Arc;

use crate::{reconcile::reconcile, router::build_router};
use sled::Db;

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{net::TcpListener, sync::mpsc};

#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    orqos: Arc<orqos_client::OrqosClient>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    tracing::info!("Starting Rezn Runtime");

    let db_path: String = env::args().nth(1).unwrap_or_else(|| "./rezn-data".into());
    let db_path_clone = db_path.clone();

    let db: Arc<Db> = Arc::new(sled::open(db_path)?);

    tracing::info!("Starting Rezn Runtime with database at: {}", db_path_clone);

    let (tx, mut rx) = mpsc::channel::<()>(1);
    let is_reconciling = Arc::new(AtomicBool::new(false));
    // let store = Arc::clone(&store);

    tracing::info!("Setting up ORQOS API client");

    let orqos_url = env::var("ORQOS_API_URL").unwrap_or_else(|_| "http://localhost:3000".into());
    // Validate URL format
    if !orqos_url.starts_with("http://") && !orqos_url.starts_with("https://") {
        return Err(anyhow::anyhow!(
            "Invalid ORQOS_API_URL: must start with http:// or https://"
        ));
    }
    let orqos = Arc::new(orqos_client::OrqosClient::new(&orqos_url));

    tracing::info!("ORQOS API client initialized with URL: {}", orqos_url);

    let app_state = Arc::new(AppState { db, orqos });

    let reconcile_state = Arc::clone(&app_state);
    let is_reconciling_clone = Arc::clone(&is_reconciling);
    tokio::spawn(async move {
        while let Some(_) = rx.recv().await {
            tracing::debug!("Received reconcile trigger");

            if is_reconciling_clone
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                tracing::debug!("[reconcile] Begin");
                if let Err(e) = reconcile(&*reconcile_state.db, &*reconcile_state.orqos).await {
                    tracing::error!("[reconcile] Error: {}", e);
                }

                is_reconciling_clone.store(false, Ordering::SeqCst);
                tracing::debug!("[reconcile] Done");
            } else {
                tracing::debug!("[reconcile] Already running — dropped request");
            }
        }
    });

    // Spawn periodic reconcile trigger
    let tx_periodic = tx.clone();
    tokio::spawn(async move {
        let interval = std::env::var("RECONCILE_INTERVAL")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(15);

        tracing::info!("Starting periodic reconcile every {} seconds", interval);

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval));
        loop {
            tracing::debug!("Triggering periodic reconcile");

            interval.tick().await;
            let _ = tx_periodic.send(()).await;
        }
    });

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:4000".into());
    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!("Listening on {}", bind_addr);

    let router = build_router(app_state);

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl-C handler");
        })
        .await?;

    Ok(())
}
