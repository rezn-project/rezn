mod docker;
mod reconcile;
mod store;
mod types;

use reconcile::reconcile_loop;
use std::{env, thread, time::Duration};
use store::SledStore;

fn main() {
    let db_path = env::args().nth(1).unwrap_or("/var/lib/rezn".into());
    let store = SledStore::new(&db_path).expect("Failed to open store");

    loop {
        if let Err(e) = reconcile_loop(&store) {
            eprintln!("Reconcile error: {}", e);
        }
        thread::sleep(Duration::from_secs(5));
    }
}
