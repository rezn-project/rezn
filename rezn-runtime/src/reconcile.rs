use crate::{
    orqos_client::OrqosClient,
    store::Store,
    types::{GenericItem, PodFields, PodSpec},
};
use anyhow::{Context, Result};
use chrono::Utc;

pub async fn reconcile(store: &(dyn Store + Send + Sync), orqos: &OrqosClient) -> Result<()> {
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

    let mut tasks = vec![];

    for pod in desired_pods {
        let running = orqos
            .list_pod_containers(&pod.name)
            .await
            .context("Failed to query Orqos for running containers")?;

        // Clone all necessary data before moving into the async block
        let pod_name = pod.name.clone();
        let pod_image = pod.image.clone();
        let pod_ports = pod.ports.clone();
        let pod_replicas = pod.replicas;
        let orqos = orqos.clone();
        let running = running.clone();

        let task = tokio::spawn(async move {
            let matches: Vec<_> = running
                .iter()
                .filter(|c| c.starts_with(&format!("{}-", pod_name)))
                .collect();

            if matches.len() < pod_replicas {
                for _ in 0..(pod_replicas - matches.len()) {
                    let cname =
                        format!("{}-{}", pod_name, Utc::now().timestamp_nanos_opt().unwrap());
                    let image = pod_image.clone();
                    let ports = pod_ports.clone();
                    let orqos = orqos.clone();
                    let parent_name = pod_name.clone();

                    tokio::spawn(async move {
                        if let Err(e) = orqos
                            .start_container(&cname, &image, &ports, &parent_name)
                            .await
                        {
                            eprintln!("Failed to start {}: {}", cname, e);
                        }
                    })
                    .await
                    .ok(); // ignore JoinError but keep the async flow non-blocking
                }
            } else if matches.len() > pod_replicas {
                for cname in matches.iter().take(matches.len() - pod_replicas) {
                    if let Err(e) = orqos.stop_container(cname).await {
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
