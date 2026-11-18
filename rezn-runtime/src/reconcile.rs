//! The controller: makes Orqos's workload pods match the stored IR v2 programs.
//!
//! One serialized pass observes every pod this database owns, plans a bounded set of changes (creations,
//! one-at-a-time replacements, removals), applies them and observes again. Ownership is proven from each pod's
//! labels and ID before anything is changed; a malformed proof stops the whole pass.
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
};

use anyhow::{bail, ensure, Context, Result};
use chrono::Utc;
use common::ir_v2::{self, Kind};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use utoipa::ToSchema;

use crate::{
    orqos_client::{ContainerStatus, OrqosClient, PodSpec, PodStatus},
    store::{self, StoredState},
    workload::{self, Workload, DEPLOYMENT, KIND, NAMESPACE, ORDINAL, OWNER, RESOURCE},
};

/// (deployment, kind, resource) of one replica set.
type WorkloadKey = (String, Kind, String);
/// Unexecuted deployments and the reasons each cannot run.
type Reasons = BTreeMap<String, Vec<String>>;

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
    pods: Vec<Owned>,
}

/// A pod proven to belong to this database.
#[derive(Clone, Debug)]
pub struct Owned {
    pub deployment: String,
    pub kind: Kind,
    pub resource: String,
    pub ordinal: u32,
    pub status: PodStatus,
}

impl Owned {
    fn workload(&self) -> WorkloadKey {
        (self.deployment.clone(), self.kind, self.resource.clone())
    }

    fn running(&self) -> bool {
        self.status.state == "running"
    }
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
    /// Deployments whose program needs features the workload backend does not provide yet. Nothing of theirs is
    /// created, changed or removed; pods of an earlier revision keep running. `converged` stays false.
    pub unexecuted: Vec<Unexecuted>,
    pub workloads: Vec<WorkloadStatus>,
}

#[derive(Serialize, ToSchema)]
pub struct Unexecuted {
    pub deployment: String,
    /// `JSON pointer: reason (issue)` for every blocking feature.
    pub reasons: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct WorkloadStatus {
    pub deployment: String,
    pub kind: String,
    pub resource: String,
    /// Null for pods awaiting removal and for deployments that are not executed.
    pub desired_replicas: Option<usize>,
    /// Null until there has been a successful observation; stale values keep their timestamp.
    pub running_replicas: Option<usize>,
    pub pods: Vec<PodView>,
}

#[derive(Serialize, ToSchema)]
pub struct PodView {
    pub id: String,
    pub ordinal: u32,
    pub hostname: String,
    /// Orqos's summary: `running`, `starting`, `restarting`, `degraded`, `failed` or `stopped`.
    pub state: String,
    /// The pod runs the desired spec of the accepted revision.
    pub current: bool,
    /// DNS names that resolve to this pod on its namespace network.
    pub aliases: Vec<String>,
    pub containers: Vec<ContainerStatus>,
    /// Orqos's last operation error for the pod.
    pub error: Option<String>,
}

/// One change to Orqos, in the order planned.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Remove(String),
    Apply(String, PodSpec),
}

/// Desired workloads of every executable deployment, and the reasons of the others.
fn desired(state: &StoredState) -> Result<(Vec<Workload>, Reasons)> {
    let mut workloads = Vec::new();
    let mut unexecuted = BTreeMap::new();
    let programs = state.desired()?;
    for (name, program) in &programs {
        // `/apply` refuses colliding names; this covers state stored before that check.
        let collisions = workload::collisions(name, program, &programs);
        match workload::translate(&state.owner, program) {
            Ok(w) if collisions.is_empty() => workloads.extend(w),
            Ok(_) => {
                unexecuted.insert(name.clone(), collisions);
            }
            Err(reasons) => {
                unexecuted.insert(name.clone(), [collisions, reasons].concat());
            }
        }
    }
    Ok((workloads, unexecuted))
}

/// Proves ownership locally: a server-side listing is no proof. Pods of other owners are ignored; a pod claiming
/// this owner with malformed labels, or an ID that does not match them, fails the observation.
pub fn owned(owner: &str, pods: Vec<PodStatus>) -> Result<Vec<Owned>> {
    let mut ids = HashSet::new();
    let mut result = Vec::new();
    for pod in pods {
        let labels = &pod.spec.labels;
        if labels.get(OWNER).map(String::as_str) != Some(owner) {
            continue;
        }
        let label = |key: &str| {
            labels
                .get(key)
                .with_context(|| format!("pod {} lacks ownership label {key}", pod.id))
        };
        let deployment = label(DEPLOYMENT)?.clone();
        let resource = label(RESOURCE)?.clone();
        ensure!(
            ir_v2::is_name(&deployment, 63)
                && ir_v2::is_name(label(NAMESPACE)?, 63)
                && ir_v2::is_name(&resource, 63),
            "pod {} has malformed ownership labels",
            pod.id
        );
        let kind = match label(KIND)?.as_str() {
            "replica_set" => Kind::ReplicaSet,
            "ordered_replica_set" => Kind::OrderedReplicaSet,
            other => bail!("pod {} has an unknown kind label {other:?}", pod.id),
        };
        let ordinal_label = label(ORDINAL)?;
        let ordinal: u32 = ordinal_label
            .parse()
            .ok()
            .filter(|n: &u32| n.to_string() == *ordinal_label)
            .with_context(|| format!("pod {} has a malformed ordinal label", pod.id))?;
        ensure!(
            pod.id == workload::pod_id(owner, &deployment, kind.as_str(), &resource, ordinal),
            "pod {} claims this owner but its ID does not match its labels",
            pod.id
        );
        ensure!(
            ids.insert(pod.id.clone()),
            "duplicate pod {} in observation",
            pod.id
        );
        result.push(Owned {
            deployment,
            kind,
            resource,
            ordinal,
            status: pod,
        });
    }
    result.sort_by_key(|p| (p.workload(), p.ordinal));
    Ok(result)
}

/// Plans one pass. Per replica set:
/// - pods beyond the replica count, or of removed resources, are removed (sequential sets: the highest ordinal
///   only, once all others run);
/// - missing ordinals are created (sequential sets: the lowest only, once all lower ones run);
/// - pods with an outdated spec are replaced: any that are not running at once, a running one (the highest
///   ordinal first) only when every other desired pod of the set exists and runs;
/// - pods Orqos reports `stopped` are applied again, which starts them.
///
/// Pods of unexecuted deployments are left alone.
pub fn plan(workloads: &[Workload], unexecuted: &Reasons, observed: &[Owned]) -> Vec<Action> {
    let mut removals = Vec::new();
    let mut applies = Vec::new();
    let mut by_workload: BTreeMap<WorkloadKey, BTreeMap<u32, &Owned>> = BTreeMap::new();
    for pod in observed {
        by_workload
            .entry(pod.workload())
            .or_default()
            .insert(pod.ordinal, pod);
    }
    let desired: BTreeSet<_> = workloads
        .iter()
        .map(|w| (w.deployment.clone(), w.kind, w.resource.clone()))
        .collect();
    for (key, pods) in &by_workload {
        if !desired.contains(key) && !unexecuted.contains_key(&key.0) {
            removals.extend(pods.values().map(|p| Action::Remove(p.status.id.clone())));
        }
    }
    for w in workloads {
        let key = (w.deployment.clone(), w.kind, w.resource.clone());
        let empty = BTreeMap::new();
        let existing = by_workload.get(&key).unwrap_or(&empty);
        let replicas = w.pods.len() as u32;
        let extra: Vec<&Owned> = existing.range(replicas..).map(|(_, p)| *p).collect();
        let others_run = |except: u32| {
            existing
                .iter()
                .filter(|(ordinal, _)| **ordinal != except)
                .all(|(_, p)| p.running())
        };
        if w.sequential {
            if let Some(highest) = extra.last() {
                if others_run(highest.ordinal) {
                    removals.push(Action::Remove(highest.status.id.clone()));
                }
            }
        } else {
            removals.extend(extra.iter().map(|p| Action::Remove(p.status.id.clone())));
        }

        let mut running_replacement = None;
        for desired in &w.pods {
            match existing.get(&desired.ordinal) {
                None => {
                    let lower_run =
                        (0..desired.ordinal).all(|o| existing.get(&o).is_some_and(|p| p.running()));
                    if !w.sequential || (lower_run && extra.is_empty()) {
                        applies.push(Action::Apply(desired.id.clone(), desired.spec.clone()));
                    }
                    if w.sequential {
                        break;
                    }
                }
                Some(pod) if pod.status.spec != desired.spec => {
                    if !pod.running() {
                        applies.push(Action::Apply(desired.id.clone(), desired.spec.clone()));
                    } else {
                        running_replacement = Some(desired);
                    }
                }
                Some(pod) if pod.status.state == "stopped" => {
                    applies.push(Action::Apply(desired.id.clone(), desired.spec.clone()));
                }
                Some(_) => {}
            }
        }
        if let Some(desired) = running_replacement {
            let all_present = w.pods.iter().all(|p| existing.contains_key(&p.ordinal));
            if all_present && extra.is_empty() && others_run(desired.ordinal) {
                applies.push(Action::Apply(desired.id.clone(), desired.spec.clone()));
            }
        }
    }
    removals.extend(applies);
    removals
}

/// Every desired pod of the executed deployments exists with its spec and runs, and they own nothing else.
fn executed_converged(workloads: &[Workload], unexecuted: &Reasons, observed: &[Owned]) -> bool {
    let desired: BTreeMap<&str, &PodSpec> = workloads
        .iter()
        .flat_map(|w| w.pods.iter().map(|p| (p.id.as_str(), &p.spec)))
        .collect();
    let executed: Vec<&Owned> = observed
        .iter()
        .filter(|p| !unexecuted.contains_key(&p.deployment))
        .collect();
    executed.len() == desired.len()
        && executed.iter().all(|p| {
            p.running()
                && desired
                    .get(p.status.id.as_str())
                    .is_some_and(|spec| **spec == p.status.spec)
        })
}

/// Converged: every deployment is executed and matches what Orqos runs.
fn converged(workloads: &[Workload], unexecuted: &Reasons, observed: &[Owned]) -> bool {
    unexecuted.is_empty() && executed_converged(workloads, unexecuted, observed)
}

impl Controller {
    pub async fn status(&self, db: &sled::Db) -> Result<Status> {
        let _intent = self.intent.lock().await;
        let state = store::load(db)?;
        let (workloads, unexecuted) = desired(&state)?;
        let progress = self.progress.read().await;
        let observed = progress.observation.as_ref();
        let pending = observed.is_some_and(|o| o.revision != state.revision);
        let mut rows: BTreeMap<WorkloadKey, (Option<&Workload>, Vec<&Owned>)> = workloads
            .iter()
            .map(|w| {
                (
                    (w.deployment.clone(), w.kind, w.resource.clone()),
                    (Some(w), vec![]),
                )
            })
            .collect();
        for pod in observed.map(|o| o.pods.as_slice()).unwrap_or_default() {
            rows.entry(pod.workload()).or_default().1.push(pod);
        }
        Ok(Status {
            owner: state.owner.clone(),
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
                && observed.is_some_and(|o| converged(&workloads, &unexecuted, &o.pods)),
            errors: progress.errors.clone(),
            unexecuted: unexecuted
                .iter()
                .map(|(deployment, reasons)| Unexecuted {
                    deployment: deployment.clone(),
                    reasons: reasons.clone(),
                })
                .collect(),
            workloads: rows
                .into_iter()
                .map(|((deployment, kind, resource), (workload, pods))| {
                    let desired_spec = |id: &str| {
                        workload.and_then(|w| w.pods.iter().find(|p| p.id == id).map(|p| &p.spec))
                    };
                    WorkloadStatus {
                        deployment,
                        kind: kind.as_str().into(),
                        resource,
                        desired_replicas: workload.map(|w| w.pods.len()),
                        running_replicas: observed
                            .map(|_| pods.iter().filter(|p| p.running()).count()),
                        pods: pods
                            .into_iter()
                            .map(|p| PodView {
                                id: p.status.id.clone(),
                                ordinal: p.ordinal,
                                hostname: p.status.spec.hostname.clone(),
                                state: p.status.state.clone(),
                                current: desired_spec(&p.status.id) == Some(&p.status.spec),
                                aliases: p
                                    .status
                                    .spec
                                    .network
                                    .as_ref()
                                    .map(|n| n.aliases.clone())
                                    .unwrap_or_default(),
                                containers: p.status.containers.clone(),
                                error: p.status.error.clone(),
                            })
                            .collect(),
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

    async fn observe(&self, state: &StoredState, orqos: &OrqosClient) -> Result<Vec<Owned>> {
        let pods = owned(
            &state.owner,
            orqos
                .list_pods()
                .await
                .context("required workload observation failed")?,
        )?;
        let mut progress = self.progress.write().await;
        progress.observation = Some(Observation {
            revision: state.revision,
            at: Utc::now().to_rfc3339(),
            pods: pods.clone(),
        });
        progress.stale = false;
        Ok(pods)
    }

    async fn pass(&self, db: &sled::Db, orqos: &OrqosClient) -> Result<()> {
        // Validate the complete persisted snapshot before observation or mutation.
        let state = store::load(db)?;
        let (workloads, unexecuted) = desired(&state)?;
        let pods = self.observe(&state, orqos).await?;
        // If apply won the race during observation, start again with its snapshot.
        ensure!(
            store::load(db)?.revision == state.revision,
            "intent changed during observation; retry pending revision"
        );
        let actions = plan(&workloads, &unexecuted, &pods);
        // Pods are independent: one failing pod must not block the others, so every action is attempted.
        let mut failures = Vec::new();
        for action in &actions {
            let result = match action {
                Action::Remove(id) => orqos.delete_pod(id).await,
                Action::Apply(id, spec) => orqos.put_pod(id, spec).await,
            };
            if let Err(error) = result {
                failures.push(format!("{error:#}"));
            }
        }
        let after = if actions.is_empty() {
            Ok(pods)
        } else {
            self.observe(&state, orqos).await
        };
        // Preserve both mutation failures and a failed follow-up observation.
        match after {
            Err(observation) => {
                failures.push(format!("follow-up observation: {observation:#}"));
                bail!("{}", failures.join("; "))
            }
            Ok(_) if !failures.is_empty() => bail!("{}", failures.join("; ")),
            // Unexecuted deployments are not errors (status lists them); the executed ones must converge.
            Ok(pods) => ensure!(
                executed_converged(&workloads, &unexecuted, &pods),
                "observed pods have not converged; retrying on next pass"
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
