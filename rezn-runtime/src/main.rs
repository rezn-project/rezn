mod age_keys;
mod intent;
mod orqos_client;
mod reconcile;
mod store;

mod router;
mod routes;
mod secret;
mod stats;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::env;
use std::sync::Arc;

use crate::{router::build_router, secret::SecretStore, stats::container_stats_handler};
use sled::Db;
use utoipa::ToSchema;

use tokio::{
    net::TcpListener,
    sync::{broadcast, RwLock},
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
struct Stats {
    cpu_avg: Option<f64>,
    max_mem: Option<u64>,
}

type ContainerID = String;

#[derive(Debug, Clone, ToSchema, Serialize)]
struct TimestampedStats {
    stats: Stats,
    timestamp: u64,
}

type StatsMap = BTreeMap<ContainerID, TimestampedStats>;

#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    orqos: Arc<orqos_client::OrqosClient>,
    stats: Arc<RwLock<StatsMap>>,
    stats_tx: broadcast::Sender<serde_json::Value>,
    secret_store: SecretStore,
    controller: Arc<reconcile::Controller>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    tracing::info!("Starting Rezn Runtime");

    let state_db_path = env::var("STATE_DB_PATH").unwrap_or_else(|_| "./rezn-data".into());
    let state_db_path_clone = state_db_path.clone();

    let db: Arc<Db> = Arc::new(sled::open(state_db_path)?);

    tracing::info!(
        "Starting Rezn Runtime with database at: {}",
        state_db_path_clone
    );

    store::initialize(&db)?;

    let (stats_tx, _) = broadcast::channel(100);

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

    let identity = age_keys::get_identity()?.clone();

    let secrets_db_path = env::var("SECRETS_DB_PATH").unwrap_or_else(|_| "./secrets".into());
    let secret_store = SecretStore::open(secrets_db_path, identity)?;

    let app_state = Arc::new(AppState {
        db,
        orqos,
        stats: Arc::new(RwLock::new(BTreeMap::default())),
        stats_tx,
        secret_store,
        controller: Arc::new(reconcile::Controller::default()),
    });

    let interval = env::var("RECONCILE_INTERVAL")
        .unwrap_or_else(|_| "15".into())
        .parse::<u64>()?;
    anyhow::ensure!(interval > 0, "RECONCILE_INTERVAL must be positive seconds");
    tokio::spawn(reconcile::run(
        app_state.clone(),
        std::time::Duration::from_secs(interval),
    ));

    let container_stats_handler_clone = Arc::clone(&app_state);

    tokio::spawn(async move {
        if let Err(e) = container_stats_handler(container_stats_handler_clone).await {
            tracing::error!("Stats handler error: {}", e);
        }
    });

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:4000".into());
    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!("Listening on {}", bind_addr);

    let router_app_state_clone = Arc::clone(&app_state);
    let router = build_router(router_app_state_clone);

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl-C handler");
        })
        .await?;

    Ok(())
}
