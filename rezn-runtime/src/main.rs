mod admission;
mod age_keys;
mod certificates;
mod intent;
mod orqos_client;
mod reconcile;
mod store;
mod workload;

mod router;
mod routes;
mod secret;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::env;
use std::sync::Arc;

use crate::{router::build_router, secret::SecretStore};
use sled::Db;
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    orqos: Arc<orqos_client::OrqosClient>,
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

    let orqos_url = env::var("ORQOS_API_URL").unwrap_or_else(|_| "http://127.0.0.1:9090".into());
    let token = env::var("ORQOS_WORKLOAD_TOKEN").map_err(|_| {
        anyhow::anyhow!(
            "ORQOS_WORKLOAD_TOKEN must be set to the Orqos workload backend's bearer token"
        )
    })?;
    let orqos = Arc::new(orqos_client::OrqosClient::new(&orqos_url, token)?);
    tracing::info!("Orqos workload API at {}", orqos_url);

    let identity = age_keys::get_identity()?.clone();

    let secrets_db_path = env::var("SECRETS_DB_PATH").unwrap_or_else(|_| "./secrets".into());
    let secret_store = SecretStore::open(secrets_db_path, identity)?;

    let app_state = Arc::new(AppState {
        db,
        orqos,
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
