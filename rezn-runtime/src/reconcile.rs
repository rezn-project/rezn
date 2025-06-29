use crate::{orqos_client::OrqosClient, store::Store};
use anyhow::{Context, Result};
use chrono::Utc;
use common::types::{DesiredMap, PodFields, PodSpec};
use std::collections::HashMap;

pub async fn reconcile(store: &(dyn Store + Send + Sync), orqos: &OrqosClient) -> Result<()> {
    tracing::debug!("Reconcile: starting");

    let data = match store.read("desired") {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("Warning: desired state not available: {}", e);
            return Ok(());
        }
    };

    tracing::debug!("Reconcile: read desired state from store");

    let desired: DesiredMap =
        serde_json::from_slice(&data).context("Failed to parse desired state as molecule map")?;

    tracing::debug!(
        "Reconcile: parsed {} items from desired state",
        desired.len()
    );

    let mut desired_pods = Vec::<PodSpec>::new();

    for (mol_name, atoms) in &desired {
        for item in atoms {
            if item.kind == "pod" {
                if let Some(_) = &item.fields {
                    let fields_val = item.fields.clone().context("pod missing 'fields'")?;
                    let fields: PodFields =
                        serde_json::from_value(fields_val).with_context(|| {
                            format!("Failed to parse pod fields in molecule '{mol_name}'")
                        })?;

                    desired_pods.push(PodSpec {
                        mol_name: mol_name.clone(),
                        name: item.name.clone(),
                        image: fields.image,
                        replicas: fields.replicas,
                        ports: fields.ports,
                    });
                }
            }
        }
    }

    let mut tasks = vec![];

    for pod in desired_pods {
        let pod_label = format!("{}:{}", pod.mol_name, pod.name);

        let running = orqos
            .list_pod_containers(&pod_label)
            .await
            .context("Failed to query Orqos for running containers")?;

        // Clone all necessary data before moving into the async block
        let mol_name = pod.mol_name.clone();
        let pod_name = pod.name.clone();
        let pod_image = pod.image.clone();
        let pod_ports = pod.ports.clone();
        let pod_replicas = pod.replicas;
        let orqos = orqos.clone();
        let mut labels: HashMap<String, String> = HashMap::new();

        labels.insert("mol".to_string(), format!("{}", pod.mol_name));
        labels.insert("pod".to_string(), pod_label.clone());

        let running = running.clone();

        let task = tokio::spawn(async move {
            let matches: Vec<_> = running
                .iter()
                .filter(|c| c.starts_with(&format!("{}-", pod_name)))
                .collect();

            if matches.len() < pod_replicas {
                for _ in 0..(pod_replicas - matches.len()) {
                    let cname = format!(
                        "{}-{}-{}",
                        mol_name,
                        pod_name,
                        Utc::now().timestamp_nanos_opt().unwrap()
                    );
                    let image = pod_image.clone();
                    let ports = pod_ports.clone();
                    let orqos = orqos.clone();

                    if let Err(e) = orqos
                        .start_container(&cname, &image, &ports, labels.clone())
                        .await
                    {
                        eprintln!("Failed to start {}: {}", cname, e);
                    }
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
