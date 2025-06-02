use crate::{
    docker,
    store::Store,
    types::{GenericItem, PodFields, PodSpec},
};
use anyhow::{Context, Result};
use std::thread;

pub fn reconcile_loop(store: &impl Store) -> Result<()> {
    let data = match store.read("desired") {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Warning: desired state not available: {}", e);
            return Ok(()); // skip this tick
        }
    };
    let items: Vec<GenericItem> = serde_json::from_slice(&data).context("parsing desired state")?;

    let mut desired_pods = vec![];
    for item in items {
        if item.kind == "pod" {
            if let Some(fields_val) = item.fields {
                let fields: PodFields =
                    serde_json::from_value(fields_val).context("parsing pod fields")?;
                desired_pods.push(PodSpec {
                    name: item.name,
                    image: fields.image,
                    replicas: fields.replicas,
                    ports: fields.ports,
                });
            }
        }
    }

    let running = docker::list_running_containers().context("listing containers")?;

    let handles: Vec<_> = desired_pods
        .into_iter()
        .map(|pod| {
            let running = running.clone();
            thread::spawn(move || {
                let matches: Vec<_> = running
                    .iter()
                    .filter(|c| c.starts_with(&format!("{}-", pod.name)))
                    .collect();
                if matches.len() < pod.replicas {
                    for _ in 0..(pod.replicas - matches.len()) {
                        let cname = format!(
                            "{}-{}",
                            pod.name,
                            chrono::Utc::now().timestamp_nanos_opt().unwrap()
                        );
                        if let Err(e) = docker::start_container(&cname, &pod.image, &pod.ports) {
                            eprintln!("Failed to start {}: {}", cname, e);
                        }
                    }
                } else if matches.len() > pod.replicas {
                    for cname in matches.iter().take(matches.len() - pod.replicas) {
                        if let Err(e) = docker::stop_container(cname) {
                            eprintln!("Failed to stop {}: {}", cname, e);
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        let _ = h.join();
    }

    Ok(())
}
