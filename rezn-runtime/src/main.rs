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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_path = env::args().nth(1).unwrap_or_else(|| "./rezn-data".into());

    let store: Arc<dyn store::Store + Send + Sync> = Arc::new(SledStore::new(&db_path)?);

    let reconciler_handle = {
        let store = Arc::clone(&store);
        task::spawn(reconcile_loop(store))
    };

    // Optional: add socket task later
    // let socket_handle = task::spawn(unix_socket_server(Arc::clone(&store)));

    tokio::select! {
        _ = signal::ctrl_c() => {
            eprintln!("SIGINT received. Shutting down.");
        }
        res = reconciler_handle => {
            if let Err(e) = res {
                eprintln!("Reconciler failed: {:?}", e);
            }
        }
        // res = socket_handle => ...
    }

    Ok(())
}
