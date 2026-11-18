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
    orqos_client::{ContainerStatus, OrqosClient, PodSpec, PodStatus, VolumeSpec, VolumeStatus},
    store::{self, StoredState},
    workload::{
        self, DesiredVolume, Workload, CLAIM, DEPLOYMENT, KIND, NAMESPACE, ORDINAL, OWNER,
        RESOURCE, RETAIN, WORKLOAD,
    },
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
    volumes: Vec<OwnedVolume>,
}

/// A volume proven to belong to this database.
#[derive(Clone, Debug)]
pub struct OwnedVolume {
    pub deployment: String,
    pub claim: String,
    /// The ordered set and ordinal of a per-replica volume.
    pub replica: Option<(String, u32)>,
    pub retain: bool,
    pub status: VolumeStatus,
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
    pub volumes: Vec<VolumeView>,
}

#[derive(Serialize, ToSchema)]
pub struct VolumeView {
    pub id: String,
    pub deployment: String,
    pub claim: String,
    /// The ordered set of a per-replica volume.
    pub workload: Option<String>,
    pub ordinal: Option<u32>,
    /// The accepted programs want it; otherwise it is retained (or awaiting deletion).
    pub desired: bool,
    /// Kept once its workload or deployment is gone.
    pub retain: bool,
    /// Observed in Orqos (false until created, or after a failed observation).
    pub exists: bool,
    /// Pods whose records mount it.
    pub mounted_by: Vec<String>,
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
    PutVolume(String, VolumeSpec),
    RemoveVolume(String),
}

/// Desired workloads and volumes of every executable deployment, and the reasons of the others.
fn desired(state: &StoredState) -> Result<(Vec<Workload>, Vec<DesiredVolume>, Reasons)> {
    let mut workloads = Vec::new();
    let mut volumes = Vec::new();
    let mut unexecuted = BTreeMap::new();
    let programs = state.desired()?;
    for (name, program) in &programs {
        // `/apply` refuses colliding names; this covers state stored before that check.
        let collisions = workload::collisions(name, program, &programs);
        match workload::translate(&state.owner, program) {
            Ok(t) if collisions.is_empty() => {
                workloads.extend(t.workloads);
                volumes.extend(t.volumes);
            }
            Ok(_) => {
                unexecuted.insert(name.clone(), collisions);
            }
            Err(reasons) => {
                unexecuted.insert(name.clone(), [collisions, reasons].concat());
            }
        }
    }
    Ok((workloads, volumes, unexecuted))
}

/// Proves volume ownership locally, like [`owned`]: volumes of other owners are ignored; one claiming this owner
/// with malformed labels, or an ID that does not match them, fails the observation.
pub fn owned_volumes(owner: &str, volumes: Vec<VolumeStatus>) -> Result<Vec<OwnedVolume>> {
    let mut result = Vec::new();
    for volume in volumes {
        let labels = &volume.spec.labels;
        if labels.get(OWNER).map(String::as_str) != Some(owner) {
            continue;
        }
        let label = |key: &str| {
            labels
                .get(key)
                .with_context(|| format!("volume {} lacks ownership label {key}", volume.name))
        };
        let deployment = label(DEPLOYMENT)?.clone();
        let claim = label(CLAIM)?.clone();
        ensure!(
            ir_v2::is_name(&deployment, 63)
                && ir_v2::is_name(label(NAMESPACE)?, 63)
                && ir_v2::is_name(&claim, 63),
            "volume {} has malformed ownership labels",
            volume.name
        );
        let retain = match label(RETAIN)?.as_str() {
            "true" => true,
            "false" => false,
            other => bail!(
                "volume {} has a malformed retain label {other:?}",
                volume.name
            ),
        };
        let replica = match (labels.get(WORKLOAD), labels.get(ORDINAL)) {
            (None, None) => None,
            (Some(set), Some(n)) => {
                let ordinal: u32 = n
                    .parse()
                    .ok()
                    .filter(|o: &u32| o.to_string() == *n)
                    .with_context(|| {
                        format!("volume {} has a malformed ordinal label", volume.name)
                    })?;
                ensure!(
                    ir_v2::is_name(set, 63),
                    "volume {} has a malformed workload label",
                    volume.name
                );
                Some((set.clone(), ordinal))
            }
            _ => bail!(
                "volume {} has only one of the workload and ordinal labels",
                volume.name
            ),
        };
        let id = workload::volume_id(
            owner,
            &deployment,
            replica.as_ref().map(|(set, _)| set.as_str()),
            &claim,
            replica.as_ref().map(|(_, n)| *n),
        );
        ensure!(
            volume.name == id,
            "volume {} claims this owner but its name does not match its labels",
            volume.name
        );
        result.push(OwnedVolume {
            deployment,
            claim,
            replica,
            retain,
            status: volume,
        });
    }
    result.sort_by(|a, b| a.status.name.cmp(&b.status.name));
    Ok(result)
}

/// Volume changes: desired volumes that are missing or whose spec differs are put (before any pod change, so pods
/// can mount them); owned volumes nobody wants are removed (after) once no pod mounts them and their policy says
/// so: a per-replica volume beyond its set's replica count when the set does not retain on scale-down, or any
/// volume whose retain label is false once its set or deployment is gone. Unexecuted deployments' volumes stay.
pub fn plan_volumes(
    workloads: &[Workload],
    volumes: &[DesiredVolume],
    unexecuted: &Reasons,
    observed: &[OwnedVolume],
) -> (Vec<Action>, Vec<Action>) {
    let by_id: BTreeMap<&str, &OwnedVolume> = observed
        .iter()
        .map(|v| (v.status.name.as_str(), v))
        .collect();
    let puts = volumes
        .iter()
        .filter(|v| {
            by_id
                .get(v.id.as_str())
                .is_none_or(|o| o.status.spec != v.spec)
        })
        .map(|v| Action::PutVolume(v.id.clone(), v.spec.clone()))
        .collect();
    let wanted: BTreeSet<&str> = volumes.iter().map(|v| v.id.as_str()).collect();
    let removes = observed
        .iter()
        .filter(|v| {
            !wanted.contains(v.status.name.as_str())
                && !unexecuted.contains_key(&v.deployment)
                && v.status.pods.is_empty()
        })
        .filter(|v| {
            let set = v.replica.as_ref().and_then(|(set, n)| {
                workloads
                    .iter()
                    .find(|w| {
                        w.deployment == v.deployment
                            && w.kind == Kind::OrderedReplicaSet
                            && &w.resource == set
                    })
                    .map(|w| (w, *n))
            });
            match set {
                Some((w, n)) => w.delete_on_scale_down && n as usize >= w.pods.len(),
                None => !v.retain,
            }
        })
        .map(|v| Action::RemoveVolume(v.status.name.clone()))
        .collect();
    (puts, removes)
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

/// Every desired volume exists with its spec and no owned volume awaits deletion.
fn volumes_converged(
    workloads: &[Workload],
    volumes: &[DesiredVolume],
    unexecuted: &Reasons,
    observed: &[OwnedVolume],
) -> bool {
    let (puts, removes) = plan_volumes(workloads, volumes, unexecuted, observed);
    puts.is_empty() && removes.is_empty()
}

/// Converged: every deployment is executed and matches what Orqos runs.
fn converged(
    workloads: &[Workload],
    volumes: &[DesiredVolume],
    unexecuted: &Reasons,
    observation: &Observation,
) -> bool {
    unexecuted.is_empty()
        && executed_converged(workloads, unexecuted, &observation.pods)
        && volumes_converged(workloads, volumes, unexecuted, &observation.volumes)
}

impl Controller {
    pub async fn status(&self, db: &sled::Db) -> Result<Status> {
        let _intent = self.intent.lock().await;
        let state = store::load(db)?;
        let (workloads, volumes, unexecuted) = desired(&state)?;
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
                && observed.is_some_and(|o| converged(&workloads, &volumes, &unexecuted, o)),
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
            volumes: {
                let observed_volumes = observed.map(|o| o.volumes.as_slice()).unwrap_or_default();
                let wanted: BTreeSet<&str> = volumes.iter().map(|v| v.id.as_str()).collect();
                let mut views: BTreeMap<String, VolumeView> = observed_volumes
                    .iter()
                    .map(|v| {
                        (
                            v.status.name.clone(),
                            VolumeView {
                                id: v.status.name.clone(),
                                deployment: v.deployment.clone(),
                                claim: v.claim.clone(),
                                workload: v.replica.as_ref().map(|(set, _)| set.clone()),
                                ordinal: v.replica.as_ref().map(|(_, n)| *n),
                                desired: wanted.contains(v.status.name.as_str()),
                                retain: v.retain,
                                exists: v.status.exists,
                                mounted_by: v.status.pods.clone(),
                            },
                        )
                    })
                    .collect();
                for v in &volumes {
                    let labels = &v.spec.labels;
                    views.entry(v.id.clone()).or_insert_with(|| VolumeView {
                        id: v.id.clone(),
                        deployment: labels[DEPLOYMENT].clone(),
                        claim: labels[CLAIM].clone(),
                        workload: labels.get(WORKLOAD).cloned(),
                        ordinal: labels.get(ORDINAL).and_then(|n| n.parse().ok()),
                        desired: true,
                        retain: labels.get(RETAIN).map(String::as_str) == Some("true"),
                        exists: false,
                        mounted_by: vec![],
                    });
                }
                views.into_values().collect()
            },
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

    async fn observe(&self, state: &StoredState, orqos: &OrqosClient) -> Result<Observation> {
        let pods = owned(
            &state.owner,
            orqos
                .list_pods()
                .await
                .context("required workload observation failed")?,
        )?;
        let volumes = owned_volumes(
            &state.owner,
            orqos
                .list_volumes()
                .await
                .context("required volume observation failed")?,
        )?;
        let observation = Observation {
            revision: state.revision,
            at: Utc::now().to_rfc3339(),
            pods,
            volumes,
        };
        let mut progress = self.progress.write().await;
        progress.observation = Some(observation.clone());
        progress.stale = false;
        Ok(observation)
    }

    async fn pass(&self, db: &sled::Db, orqos: &OrqosClient) -> Result<()> {
        // Validate the complete persisted snapshot before observation or mutation.
        let state = store::load(db)?;
        let (workloads, volumes, unexecuted) = desired(&state)?;
        let observation = self.observe(&state, orqos).await?;
        // If apply won the race during observation, start again with its snapshot.
        ensure!(
            store::load(db)?.revision == state.revision,
            "intent changed during observation; retry pending revision"
        );
        // Volumes first (pods mount them), then pods, then volume removals (only volumes no pod mounts).
        let (mut actions, removals) =
            plan_volumes(&workloads, &volumes, &unexecuted, &observation.volumes);
        actions.extend(plan(&workloads, &unexecuted, &observation.pods));
        actions.extend(removals);
        // Pods are independent: one failing pod must not block the others, so every action is attempted.
        let mut failures = Vec::new();
        for action in &actions {
            let result = match action {
                Action::Remove(id) => orqos.delete_pod(id).await,
                Action::Apply(id, spec) => orqos.put_pod(id, spec).await,
                Action::PutVolume(id, spec) => orqos.put_volume(id, spec).await,
                Action::RemoveVolume(id) => orqos.delete_volume(id).await,
            };
            if let Err(error) = result {
                failures.push(format!("{error:#}"));
            }
        }
        let after = if actions.is_empty() {
            Ok(observation)
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
            Ok(after) => ensure!(
                executed_converged(&workloads, &unexecuted, &after.pods)
                    && volumes_converged(&workloads, &volumes, &unexecuted, &after.volumes),
                "observed pods and volumes have not converged; retrying on next pass"
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
