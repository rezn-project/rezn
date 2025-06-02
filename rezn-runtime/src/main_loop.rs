use crate::{reconcile::reconcile, store::Store};
use std::env;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub async fn reconcile_loop(store: Arc<dyn Store + Send + Sync>) {
    let interval = env::var("RECONCILE_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5);

    loop {
        if let Err(e) = reconcile(store.as_ref()).await {
            eprintln!("Reconcile error: {}", e);
        }
        sleep(Duration::from_secs(interval)).await;
    }
}
