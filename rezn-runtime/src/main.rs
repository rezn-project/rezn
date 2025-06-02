mod docker;
mod main_loop;
mod reconcile;
mod store;
mod types;

use std::env;
use std::sync::Arc;

use crate::main_loop::reconcile_loop;
use store::SledStore;
use tokio::{signal, task};

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_path = env::args().nth(1).unwrap_or_else(|| "./rezn-data".into());

    let store: Arc<dyn store::Store + Send + Sync> = Arc::new(SledStore::new(&db_path)?);

    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
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
                if let Err(e) = reconcile_loop(store).await {
                    eprintln!("[reconcile] Error: {}", e);
                }
                is_reconciling_clone.store(false, Ordering::SeqCst);
                eprintln!("[reconcile] Done");
            } else {
                eprintln!("[reconcile] Already running — dropped request");
            }
        }
    });

    Ok(())
}
