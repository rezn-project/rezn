mod docker;
mod reconcile;
mod store;
mod types;

use reconcile::reconcile_loop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{env, thread, time::Duration};
use store::SledStore;

fn main() {
    let db_path = env::args().nth(1).unwrap_or("./rezn-data".into());
    let store = match SledStore::new(&db_path) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("Failed to open store at '{}': {}", db_path, e);
            std::process::exit(1);
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    while running.load(Ordering::SeqCst) {
        if let Err(e) = reconcile_loop(&store) {
            eprintln!("Reconcile error: {}", e);
        }
        let sleep_duration = env::var("RECONCILE_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        thread::sleep(Duration::from_secs(sleep_duration));
    }
}
