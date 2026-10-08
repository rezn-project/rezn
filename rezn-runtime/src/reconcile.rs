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

    // Finish observing every workload before starting any mutations.
    let mut observed_pods = vec![];
    for pod in desired_pods {
        let pod_label = format!("{}:{}", pod.mol_name, pod.name);

        let running = orqos
            .list_pod_containers(&pod_label)
            .await
            .context("Failed to query Orqos for running containers")?;

        observed_pods.push((pod, pod_label, running));
    }

    let mut tasks = vec![];
    for (pod, pod_label, running) in observed_pods {
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
                        tracing::warn!("Failed to start {}: {:#}", cname, e);
                    }
                }
            } else if matches.len() > pod_replicas {
                for c in matches.iter().take(matches.len() - pod_replicas) {
                    if let Some(name) = c.names.first().map(|s| s.trim_start_matches('/')) {
                        if let Err(e) = orqos.stop_container(name).await {
                            tracing::warn!("Failed to stop {}: {:#}", name, e);
                        }

                        if let Err(e) = orqos.remove_container(name).await {
                            tracing::warn!("Failed to remove {}: {:#}", name, e);
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

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

    use anyhow::{ensure, Context};
    use axum::http::{Method, StatusCode};
    use serde_json::json;

    use super::*;
    use crate::{orqos_client::ContainerSummary, test_support::MockOrqos};

    fn desired_pod(image: &str, replicas: usize) -> serde_json::Value {
        json!([{"kind": "pod", "name": "web", "fields": {
            "image": image, "replicas": replicas, "ports": []
        }}])
    }

    #[tokio::test]
    async fn failed_list_prevents_all_mutations_and_next_pass_can_recover() {
        // Exercise both scale-up and scale-down of the first workload when
        // observation of a later workload fails.
        for scale_up in [true, false] {
            let unavailable = Arc::new(AtomicBool::new(true));
            let failed = unavailable.clone();
            let server = MockOrqos::start(move |request| {
                if request.method == Method::GET {
                    if request.query().get("label").map(String::as_str) == Some("pod=b:web") {
                        return if failed.load(Ordering::SeqCst) {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                "Docker temporarily unavailable".into(),
                            )
                        } else {
                            (StatusCode::OK, "[]".into())
                        };
                    }
                    return (
                        StatusCode::OK,
                        if scale_up {
                            "[]".into()
                        } else {
                            json!([{"Id": "existing", "Names": ["/a-web-existing"]}]).to_string()
                        },
                    );
                }
                (StatusCode::NO_CONTENT, String::new())
            })
            .await;
            let db = sled::Config::new().temporary(true).open().unwrap();
            db.insert(
                "desired",
                serde_json::to_vec(&json!({
                    "a": desired_pod("nginx:alpine", usize::from(scale_up)),
                    "b": desired_pod("nginx:alpine", 0),
                }))
                .unwrap(),
            )
            .unwrap();

            let error = format!("{:#}", reconcile(&db, &server.client).await.unwrap_err());
            assert!(
                error.contains("503") && error.contains("Docker temporarily unavailable"),
                "{error}"
            );
            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            assert!(requests.iter().all(|request| request.method == Method::GET));

            unavailable.store(false, Ordering::SeqCst);
            reconcile(&db, &server.client).await.unwrap();
            let requests = server.requests();
            assert_eq!(requests.len(), if scale_up { 5 } else { 6 });
            assert!(requests[..4]
                .iter()
                .all(|request| request.method == Method::GET));
            let paths: Vec<_> = requests[4..]
                .iter()
                .map(|request| request.uri.path())
                .collect();
            if scale_up {
                assert_eq!(paths, ["/docker/containers"]);
            } else {
                assert_eq!(
                    paths,
                    [
                        "/docker/containers/a-web-existing/stop",
                        "/docker/containers/a-web-existing/remove"
                    ]
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires local Orqos with Docker enabled and pre-pulled nginx:alpine"]
    async fn docker_reconciliation_smoke() -> Result<()> {
        let url = std::env::var("ORQOS_API_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
        let endpoint = url::Url::parse(&url)?;
        let loopback = match endpoint.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        ensure!(
            loopback,
            "Docker smoke tests require a loopback Orqos endpoint"
        );
        let client = OrqosClient::new(&url);
        let run = format!(
            "rezn-smoke-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        );
        let label = format!("{run}:web");
        let db = sled::Config::new().temporary(true).open()?;

        // Capture failures as Results so cleanup still runs after failed checks.
        let scenario: Result<()> = async {
            for replicas in [1, 2, 1, 0] {
                db.insert("desired", serde_json::to_vec(&json!({
                    run.clone(): desired_pod("nginx:alpine", replicas)
                }))?)?;
                reconcile(&db, &client).await?;
                let before = client.list_pod_containers(&label).await?;
                ensure!(before.len() == replicas,
                    "Expected {replicas} replicas, found {}; pre-pull nginx:alpine into Orqos's Docker engine",
                    before.len());
                reconcile(&db, &client).await?;
                let after = client.list_pod_containers(&label).await?;
                let mut before_ids: Vec<_> = before.iter().map(|container| &container.id).collect();
                let mut after_ids: Vec<_> = after.iter().map(|container| &container.id).collect();
                before_ids.sort();
                after_ids.sort();
                ensure!(before_ids == after_ids, "Repeated reconciliation changed replica identities");
            }
            Ok(())
        }.await;

        let cleanup = cleanup_smoke_containers(&url, &label, &run, &client).await;
        match (scenario, cleanup) {
            (Err(scenario), Err(cleanup)) => {
                anyhow::bail!("Smoke check failed: {scenario:#}; cleanup failed: {cleanup:#}")
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(()), cleanup) => cleanup.context("Failed to clean up Docker smoke fixtures"),
        }
    }

    async fn cleanup_smoke_containers(
        url: &str,
        label: &str,
        run: &str,
        client: &OrqosClient,
    ) -> Result<()> {
        // Include stopped containers and containers whose start request failed.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        let list = http
            .get(format!("{url}/docker/containers"))
            .query(&[("label", format!("pod={label}")), ("all", "true".into())]);
        let containers = list
            .try_clone()
            .unwrap()
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<ContainerSummary>>()
            .await?;
        let mut failures = vec![];
        for container in containers {
            ensure!(
                container.names.iter().any(|name| name
                    .trim_start_matches('/')
                    .starts_with(&format!("{run}-web-"))),
                "Refusing cleanup of container outside this smoke run: {}",
                container.id
            );
            if let Err(error) = client.remove_container(&container.id).await {
                failures.push(format!("{error:#}"));
            }
        }
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        let remaining = list
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<ContainerSummary>>()
            .await?;
        ensure!(remaining.is_empty(), "Smoke fixtures remain after cleanup");
        Ok(())
    }
}
