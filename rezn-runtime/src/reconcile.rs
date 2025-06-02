use crate::{
    docker,
    store::Store,
    types::{GenericItem, PodFields, PodSpec},
};
use anyhow::{Context, Result};
use chrono::Utc;

pub async fn reconcile(store: &(dyn Store + Send + Sync)) -> Result<()> {
    let data = match store.read("desired") {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Warning: desired state not available: {}", e);
            return Ok(());
        }
    };

    let items: Vec<GenericItem> =
        serde_json::from_slice(&data).context("Failed to parse desired state from store")?;

    let mut desired_pods = vec![];
    for item in items {
        if item.kind == "pod" {
            if let Some(fields_val) = item.fields {
                let fields: PodFields =
                    serde_json::from_value(fields_val).context("Failed to parse pod fields")?;
                desired_pods.push(PodSpec {
                    name: item.name,
                    image: fields.image,
                    replicas: fields.replicas,
                    ports: fields.ports,
                });
            }
        }
    }

    let running = docker::list_running_containers().context("Failed to list running containers")?;

    let mut tasks = vec![];

    for pod in desired_pods {
        let running = running.clone();
        let task = tokio::spawn(async move {
            let matches: Vec<_> = running
                .iter()
                .filter(|c| c.starts_with(&format!("{}-", pod.name)))
                .collect();

            if matches.len() < pod.replicas {
                for _ in 0..(pod.replicas - matches.len()) {
                    let cname =
                        format!("{}-{}", pod.name, Utc::now().timestamp_nanos_opt().unwrap());
                    let image = pod.image.clone();
                    let ports = pod.ports.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = docker::start_container(&cname, &image, &ports) {
                            eprintln!("Failed to start {}: {}", cname, e);
                        }
                    })
                    .await
                    .ok(); // ignore JoinError but keep the async flow non-blocking
                }
            } else if matches.len() > pod.replicas {
                for cname in matches.iter().take(matches.len() - pod.replicas) {
                    if let Err(e) = docker::stop_container(cname) {
                        eprintln!("Failed to stop {}: {}", cname, e);
                    }
                }
            }
        });

        tasks.push(task);
    }

    // Await all pod reconcile tasks
    for task in tasks {
        if let Err(e) = task.await {
            eprintln!("Pod reconcile task failed: {}", e);
        }
    }

    Ok(())
}
