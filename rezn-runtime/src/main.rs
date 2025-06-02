mod docker;
mod reconcile;
mod store;
mod types;

use std::env;
use std::sync::Arc;

use crate::reconcile::reconcile;
use store::SledStore;

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_path = env::args().nth(1).unwrap_or_else(|| "./rezn-data".into());

    let store: Arc<dyn store::Store + Send + Sync> = Arc::new(SledStore::new(&db_path)?);

    let (tx, mut rx) = mpsc::channel::<()>(1);
    let is_reconciling = Arc::new(AtomicBool::new(false));
    let store = Arc::clone(&store);

    // Spawn reconcile listener
    let is_reconciling_clone = Arc::clone(&is_reconciling);
    tokio::spawn(async move {
        while let Some(_) = rx.recv().await {
            if is_reconciling_clone
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                eprintln!("[reconcile] Begin");
                if let Err(e) = reconcile(&*store).await {
                    eprintln!("[reconcile] Error: {}", e);
                }

                is_reconciling_clone.store(false, Ordering::SeqCst);
                eprintln!("[reconcile] Done");
            } else {
                eprintln!("[reconcile] Already running — dropped request");
            }
        }
    });

    // Spawn periodic reconcile trigger
    let tx_periodic = tx.clone();
    tokio::spawn(async move {
        let interval = std::env::var("RECONCILE_INTERVAL")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval));
        loop {
            interval.tick().await;
            let _ = tx_periodic.send(());
        }
    });

    // Handle Ctrl-C gracefully
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");
    eprintln!("Received Ctrl-C, shutting down...");

    Ok(())
}
