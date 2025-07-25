use crate::orqos_client::{CreateReq, OrqosClient, PortMap};
use anyhow::{Context, Result};
use chrono::Utc;
use common::types::{DesiredMap, PodFields, PodSpec};
use sled::Db;
use std::collections::HashMap;

pub async fn reconcile(db: &Db, orqos: &OrqosClient) -> Result<()> {
    tracing::debug!("Reconcile: starting");

    let data = match db.get("desired") {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            tracing::debug!("Warning: 'desired' state not found in the DB");
            return Ok(()); // or return Err(...) if it's mandatory
        }
        Err(e) => {
            tracing::warn!("Warning: failed to read 'desired' state: {}", e);
            return Ok(());
        }
    };

    tracing::debug!("Reconcile: read desired state from store");

    let desired: DesiredMap = serde_json::from_slice(&data)
        .context("Failed to parse desired state as instruction map")?;

    tracing::debug!(
        "Reconcile: parsed {} items from desired state",
        desired.len()
    );

    let mut desired_pods = Vec::<PodSpec>::new();

    for (mol_name, atoms) in &desired {
        for item in atoms {
            if item.kind == "pod" {
                if let Some(fields_val) = &item.fields {
                    let fields: PodFields = serde_json::from_value(fields_val.clone())
                        .with_context(|| {
                            format!("Failed to parse pod fields in instruction '{mol_name}'")
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
                .filter(|c| {
                    c.names.iter().any(|n| {
                        n.trim_start_matches('/')
                            .starts_with(&format!("{}-{}-", mol_name, pod_name))
                    })
                })
                .collect();

            if matches.len() < pod_replicas {
                for _ in 0..(pod_replicas - matches.len()) {
                    let cname: String = format!(
                        "{}-{}-{}",
                        mol_name,
                        pod_name,
                        Utc::now().timestamp_nanos_opt().unwrap_or_default()
                    );
                    let image = pod_image.clone();
                    let ports = pod_ports.clone();
                    let orqos = orqos.clone();

                    let port_maps: Vec<PortMap> = ports
                        .iter()
                        .map(|p| PortMap {
                            container: *p,
                            host: 0,
                        })
                        .collect();

                    let req = CreateReq {
                        name: cname.clone(),
                        image,
                        ports: port_maps,
                        labels: labels.clone(),
                        cpu: None,
                    };

                    if let Err(e) = orqos.start_container(req).await {
                        tracing::warn!("Failed to start {}: {}", cname, e);
                    }
                }
            } else if matches.len() > pod_replicas {
                for c in matches.iter().take(matches.len() - pod_replicas) {
                    if let Some(name) = c.names.first().map(|s| s.trim_start_matches('/')) {
                        if let Err(e) = orqos.stop_container(name).await {
                            tracing::warn!("Failed to stop {}: {}", name, e);
                        }

                        if let Err(e) = orqos.remove_container(name).await {
                            tracing::warn!("Failed to remove {}: {}", name, e);
                        }
                    } else {
                        tracing::warn!("Container {} has no name?!", c.id);
                    }
                }
            }
        });

        tasks.push(task);
    }

    // Await all pod reconcile tasks
    for task in tasks {
        if let Err(e) = task.await {
            tracing::warn!("Pod reconcile task failed: {}", e);
        }
    }

    Ok(())
}
