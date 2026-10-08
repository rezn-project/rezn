use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use anyhow::{ensure, Context, Result};
use chrono::Utc;
use common::types::{DesiredMap, PodFields};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use utoipa::ToSchema;

use crate::{
    intent::{configuration, validate_name},
    orqos_client::{ContainerSummary, CreateReq, OrqosClient, PortMap},
    store::{self, StoredState},
};

pub const OWNER: &str = "dev.rezn.owner";
pub const MANAGED: &str = "dev.rezn.managed";
pub const DEPLOYMENT: &str = "dev.rezn.deployment";
pub const POD: &str = "dev.rezn.pod";
pub const CONFIG: &str = "dev.rezn.configuration";

#[derive(Clone, Default)]
struct Progress {
    observation: Option<Observation>,
    last_attempt: Option<String>,
    stale: bool,
    errors: Vec<String>,
}
#[derive(Clone)]
struct Observation {
    revision: u64,
    at: String,
    containers: Vec<ContainerSummary>,
}

#[derive(Default)]
pub struct Controller {
    // Serializes passes, including observation and status publication.
    gate: Mutex<()>,
    // Apply and status share this lock so status describes one accepted revision.
    pub intent: Mutex<()>,
    progress: RwLock<Progress>,
    pub trigger: tokio::sync::Notify,
}

#[derive(Serialize, ToSchema)]
pub struct Status {
    pub owner: String,
    pub desired_revision: u64,
    pub observed_revision: Option<u64>,
    pub last_observation: Option<String>,
    pub last_attempt: Option<String>,
    pub observation: String,
    pub converged: bool,
    pub errors: Vec<String>,
    pub workloads: Vec<WorkloadStatus>,
}
#[derive(Serialize, ToSchema)]
pub struct WorkloadStatus {
    pub deployment: String,
    pub pod: String,
    pub desired_configuration: Option<String>,
    pub desired: Option<PodFields>,
    pub current_configurations: Vec<String>,
    pub desired_replicas: usize,
    /// Null until there has been a successful observation; stale values retain their timestamp.
    pub running_replicas: Option<usize>,
    pub containers: Vec<ContainerSummary>,
}

fn desired_workloads(desired: &DesiredMap) -> BTreeMap<(String, String), PodFields> {
    desired
        .iter()
        .flat_map(|(deployment, instructions)| {
            instructions
                .iter()
                .map(move |i| ((deployment.clone(), i.name.clone()), i.fields.clone()))
        })
        .collect()
}

// A server-side label filter is not ownership proof. Check returned labels locally.
fn owned(state: &StoredState, containers: Vec<ContainerSummary>) -> Result<Vec<ContainerSummary>> {
    let mut ids = HashSet::new();
    let mut result = vec![];
    for container in containers {
        if container.labels.get(OWNER) != Some(&state.owner)
            || container.labels.get(MANAGED).map(String::as_str) != Some("v1")
        {
            continue;
        }
        for label in [DEPLOYMENT, POD] {
            validate_name(container.labels.get(label).with_context(|| {
                format!("container {} lacks ownership label {label}", container.id)
            })?)?;
        }
        ensure!(
            container
                .labels
                .get(CONFIG)
                .is_some_and(|s| store::is_identity(s)),
            "container {} lacks a valid configuration identity",
            container.id
        );
        ensure!(
            !container.id.is_empty()
                && container.id.len() <= 128
                && container
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid container ID in observation"
        );
        ensure!(
            ids.insert(container.id.clone()),
            "duplicate container ID in observation"
        );
        ensure!(
            !container.image.is_empty()
                && [
                    "created",
                    "restarting",
                    "running",
                    "removing",
                    "paused",
                    "exited",
                    "dead"
                ]
                .contains(&container.state.as_str()),
            "invalid container image/state in observation"
        );
        ensure!(
            container.ports.iter().all(|p| p.private_port > 0
                && ["tcp", "udp", "sctp"].contains(&p.protocol.as_str())
                && p.public_port.is_none_or(|port| port > 0
                    && p.ip
                        .as_ref()
                        .is_some_and(|ip| ip.parse::<std::net::IpAddr>().is_ok()))),
            "invalid port mapping in container {} observation",
            container.id
        );
        result.push(container);
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}

fn key(container: &ContainerSummary) -> (String, String) {
    (
        container.labels[DEPLOYMENT].clone(),
        container.labels[POD].clone(),
    )
}

fn matching(container: &ContainerSummary, fields: &PodFields) -> bool {
    container.state == "running"
        && container.labels[CONFIG] == configuration(fields)
        && container.image == fields.image
        && fields.ports.iter().all(|port| {
            container.ports.iter().any(|mapping| {
                mapping.private_port == *port
                    && mapping.protocol == "tcp"
                    && mapping.public_port.is_some_and(|p| p > 0)
            })
        })
}

fn converged(desired: &DesiredMap, containers: &[ContainerSummary]) -> bool {
    let workloads = desired_workloads(desired);
    containers
        .iter()
        .all(|c| workloads.get(&key(c)).is_some_and(|f| matching(c, f)))
        && workloads.iter().all(|(k, f)| {
            containers
                .iter()
                .filter(|c| &key(c) == k && matching(c, f))
                .count()
                == f.replicas
        })
}

impl Controller {
    pub async fn status(&self, db: &sled::Db) -> Result<Status> {
        let _intent = self.intent.lock().await;
        let state = store::load(db)?;
        let desired = state.desired()?;
        let progress = self.progress.read().await;
        let mut workloads = desired_workloads(&desired)
            .into_iter()
            .map(|(key, fields)| (key, Some(fields)))
            .collect::<BTreeMap<_, _>>();
        if let Some(observation) = &progress.observation {
            for c in &observation.containers {
                workloads.entry(key(c)).or_insert(None);
            }
        }
        let observed = progress.observation.as_ref();
        let pending = observed.is_some_and(|o| o.revision != state.revision);
        Ok(Status {
            owner: state.owner,
            desired_revision: state.revision,
            observed_revision: observed.map(|o| o.revision),
            last_observation: observed.map(|o| o.at.clone()),
            last_attempt: progress.last_attempt.clone(),
            observation: if observed.is_none() {
                "unknown"
            } else if progress.stale {
                "stale"
            } else if pending {
                "pending"
            } else {
                "fresh"
            }
            .into(),
            converged: !progress.stale
                && !pending
                && progress.errors.is_empty()
                && observed.is_some_and(|o| converged(&desired, &o.containers)),
            errors: progress.errors.clone(),
            workloads: workloads
                .into_iter()
                .map(|((deployment, pod), fields)| {
                    let containers: Vec<_> = observed
                        .map(|o| {
                            o.containers
                                .iter()
                                .filter(|c| key(c) == (deployment.clone(), pod.clone()))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut configurations = containers
                        .iter()
                        .map(|c| c.labels[CONFIG].clone())
                        .collect::<Vec<_>>();
                    configurations.sort();
                    configurations.dedup();
                    WorkloadStatus {
                        deployment,
                        pod,
                        desired_configuration: fields.as_ref().map(configuration),
                        desired_replicas: fields.as_ref().map(|f| f.replicas).unwrap_or(0),
                        desired: fields,
                        current_configurations: configurations,
                        running_replicas: observed
                            .map(|_| containers.iter().filter(|c| c.state == "running").count()),
                        containers,
                    }
                })
                .collect(),
        })
    }

    pub async fn reconcile(&self, db: &sled::Db, orqos: &OrqosClient) -> Result<()> {
        let _pass = self.gate.lock().await;
        self.progress.write().await.last_attempt = Some(Utc::now().to_rfc3339());
        let result = self.pass(db, orqos).await;
        if let Err(error) = &result {
            let mut progress = self.progress.write().await;
            progress.stale = true;
            progress.errors = vec![format!("{error:#}")];
        }
        result
    }

    async fn observe(
        &self,
        state: &StoredState,
        orqos: &OrqosClient,
    ) -> Result<Vec<ContainerSummary>> {
        let containers = owned(
            state,
            orqos
                .list_owned_containers(&state.owner)
                .await
                .context("required Docker observation failed")?,
        )?;
        let mut progress = self.progress.write().await;
        progress.observation = Some(Observation {
            revision: state.revision,
            at: Utc::now().to_rfc3339(),
            containers: containers.clone(),
        });
        progress.stale = false;
        Ok(containers)
    }

    async fn pass(&self, db: &sled::Db, orqos: &OrqosClient) -> Result<()> {
        // Validate the complete persisted snapshot before observation or mutation.
        let state = store::load(db)?;
        let desired = state.desired()?;
        let workloads = desired_workloads(&desired);
        let containers = self.observe(&state, orqos).await?;
        // If apply won the race during observation, start again with its snapshot.
        ensure!(
            store::load(db)?.revision == state.revision,
            "intent changed during observation; retry pending revision"
        );
        let mut keep: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut remove = vec![];
        for c in &containers {
            let k = key(c);
            let count = keep.entry(k.clone()).or_default();
            if workloads
                .get(&k)
                .is_some_and(|f| matching(c, f) && *count < f.replicas)
            {
                *count += 1;
            } else {
                remove.push(c);
            }
        }
        let mutated = !remove.is_empty()
            || workloads
                .iter()
                .any(|(k, f)| keep.get(k).copied().unwrap_or(0) < f.replicas);
        let mutations: Result<()> = async {
            for c in remove {
                // Force removal handles running, stopped and failed-start debris alike.
                orqos
                    .remove_container(&c.id)
                    .await
                    .with_context(|| format!("removing owned container {}", c.id))?;
            }
            for ((deployment, pod), fields) in &workloads {
                for _ in keep
                    .get(&(deployment.clone(), pod.clone()))
                    .copied()
                    .unwrap_or(0)..fields.replicas
                {
                    let labels = HashMap::from([
                        (OWNER.into(), state.owner.clone()),
                        (MANAGED.into(), "v1".into()),
                        (DEPLOYMENT.into(), deployment.clone()),
                        (POD.into(), pod.clone()),
                        (CONFIG.into(), configuration(fields)),
                    ]);
                    orqos
                        .start_container(CreateReq {
                            name: format!("rezn-{}", store::random_id()?),
                            image: fields.image.clone(),
                            cpu: None,
                            ports: fields
                                .ports
                                .iter()
                                .map(|p| PortMap {
                                    container: *p,
                                    host: 0,
                                })
                                .collect(),
                            labels,
                        })
                        .await
                        .with_context(|| format!("creating {deployment}/{pod}"))?;
                }
            }
            Ok(())
        }
        .await;
        let after = if mutated {
            self.observe(&state, orqos).await
        } else {
            Ok(containers)
        };
        // Preserve both a mutation failure and a failed follow-up observation.
        match (mutations, after) {
            (Err(mutation), Err(observation)) => {
                anyhow::bail!("{mutation:#}; follow-up observation: {observation:#}")
            }
            (Err(error), _) | (_, Err(error)) => return Err(error),
            (Ok(()), Ok(containers)) => ensure!(
                converged(&desired, &containers),
                "observed containers have not converged; retrying on next pass"
            ),
        }
        self.progress.write().await.errors.clear();
        Ok(())
    }
}

pub async fn run(app: Arc<crate::AppState>, interval: std::time::Duration) {
    let mut timer = tokio::time::interval(interval);
    loop {
        tokio::select! { _ = timer.tick() => {}, _ = app.controller.trigger.notified() => {} }
        if let Err(error) = app.controller.reconcile(&app.db, &app.orqos).await {
            tracing::error!("reconciliation failed: {error:#}");
        }
    }
}
