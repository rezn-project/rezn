//! Admission against host capacity (rezn#40). On one host, requests cannot choose where pods go, but they can refuse
//! a revision that does not fit instead of starting workloads that then starve or are killed.
//!
//! A pod requests what Kubernetes calls its effective request: the larger of its biggest init container and the sum
//! of its sidecars and containers (init containers run before the others). A request left out defaults to its limit.
//! A deployment requests the sum over its replica sets and ordered sets of pod request × replicas; jobs are not
//! executed yet (rezn#42) and are not counted. Allocatable is the host's capacity minus a reserve for the host itself.
use std::collections::BTreeMap;

use common::ir_v2::{self, Program, Spec};
use serde::Serialize;
use utoipa::ToSchema;

use crate::orqos_client::Capacity;

/// CPU and memory, as requested or available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct Totals {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
}

impl std::ops::Add for Totals {
    type Output = Totals;
    fn add(self, other: Totals) -> Totals {
        Totals {
            cpu_millis: self.cpu_millis.saturating_add(other.cpu_millis),
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
        }
    }
}

impl std::iter::Sum for Totals {
    fn sum<I: Iterator<Item = Totals>>(iter: I) -> Totals {
        iter.fold(Totals::default(), |a, b| a + b)
    }
}

fn container(c: &ir_v2::Container) -> Totals {
    let r = c.resources.as_ref();
    Totals {
        cpu_millis: r
            .and_then(|r| r.cpu_request_millis.or(r.cpu_limit_millis))
            .unwrap_or(0),
        memory_bytes: r
            .and_then(|r| r.memory_request_bytes.or(r.memory_limit_bytes))
            .unwrap_or(0),
    }
}

/// A pod's effective request: max(largest init container, sidecars + containers), per resource.
pub fn pod(pod: &ir_v2::Pod) -> Totals {
    let running: Totals = pod
        .sidecars
        .iter()
        .chain(&pod.containers)
        .map(container)
        .sum();
    let init = pod
        .init
        .iter()
        .map(container)
        .fold(Totals::default(), |a, b| Totals {
            cpu_millis: a.cpu_millis.max(b.cpu_millis),
            memory_bytes: a.memory_bytes.max(b.memory_bytes),
        });
    Totals {
        cpu_millis: running.cpu_millis.max(init.cpu_millis),
        memory_bytes: running.memory_bytes.max(init.memory_bytes),
    }
}

/// What each replica set and ordered set of a program requests (pod request × replicas), by name.
pub fn workloads(program: &Program) -> Vec<(String, u32, Totals)> {
    program
        .resources
        .iter()
        .filter_map(|r| {
            let (replicas, p) = match &r.spec {
                Spec::ReplicaSet(s) => (s.replicas, &s.pod),
                Spec::OrderedReplicaSet(s) => (s.replicas, &s.pod),
                _ => return None,
            };
            let each = pod(p);
            let total = Totals {
                cpu_millis: each.cpu_millis.saturating_mul(replicas.into()),
                memory_bytes: each.memory_bytes.saturating_mul(replicas.into()),
            };
            Some((r.name.clone(), replicas, total))
        })
        .collect()
}

pub fn program(program: &Program) -> Totals {
    workloads(program).into_iter().map(|(_, _, t)| t).sum()
}

/// What Rezn leaves to the host: `REZN_RESERVED_CPU_MILLIS` (default 250) and `REZN_RESERVED_MEMORY_BYTES` (default
/// 512 MiB).
pub fn reserve() -> Totals {
    let read = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    Totals {
        cpu_millis: read("REZN_RESERVED_CPU_MILLIS", 250),
        memory_bytes: read("REZN_RESERVED_MEMORY_BYTES", 512 << 20),
    }
}

pub fn allocatable(capacity: Capacity, reserve: Totals) -> Totals {
    Totals {
        cpu_millis: capacity.cpu_millis.saturating_sub(reserve.cpu_millis),
        memory_bytes: capacity.memory_bytes.saturating_sub(reserve.memory_bytes),
    }
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes.div_ceil(1 << 20))
}

/// Refuses `program` (deployment `name`) when it and every other stored deployment request more than `allocatable`,
/// naming the resource, the totals and the program's biggest workloads.
pub fn check(
    name: &str,
    program: &Program,
    others: &BTreeMap<String, Program>,
    allocatable: Totals,
) -> Result<(), String> {
    let mine = self::program(program);
    let rest: Totals = others
        .iter()
        .filter(|(other, _)| *other != name)
        .map(|(_, p)| self::program(p))
        .sum();
    let total = mine + rest;
    let mut problems = Vec::new();
    let mut workloads = workloads(program);
    let largest = |key: fn(&Totals) -> u64,
                   show: fn(u64) -> String,
                   workloads: &mut Vec<(String, u32, Totals)>| {
        workloads.sort_by_key(|(_, _, t)| std::cmp::Reverse(key(t)));
        workloads
            .iter()
            .filter(|(_, _, t)| key(t) > 0)
            .take(3)
            .map(|(w, n, t)| format!("{w} {n} × {}", show(key(t) / u64::from((*n).max(1)))))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if total.cpu_millis > allocatable.cpu_millis {
        problems.push(format!(
            "CPU: {}m requested of {}m allocatable (deployment {name:?} {}m: {}; other deployments {}m)",
            total.cpu_millis,
            allocatable.cpu_millis,
            mine.cpu_millis,
            largest(|t| t.cpu_millis, |v| format!("{v}m"), &mut workloads),
            rest.cpu_millis
        ));
    }
    if total.memory_bytes > allocatable.memory_bytes {
        problems.push(format!(
            "memory: {} requested of {} allocatable (deployment {name:?} {}: {}; other deployments {})",
            mib(total.memory_bytes),
            mib(allocatable.memory_bytes),
            mib(mine.memory_bytes),
            largest(|t| t.memory_bytes, mib, &mut workloads),
            mib(rest.memory_bytes)
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "requests exceed the host's allocatable capacity: {}. Lower requests or replicas, or remove other deployments.",
            problems.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn program(name: &str, workloads: serde_json::Value) -> Program {
        ir_v2::validate(&json!({"ir": "rezn.dev/v2", "deployment": name, "namespace": "ns", "resources": workloads}))
            .unwrap_or_else(|issues| panic!("{}", ir_v2::describe(&issues)))
    }

    fn set(name: &str, replicas: u32, pod: serde_json::Value) -> serde_json::Value {
        json!({"kind": "replica_set", "name": name, "fields": {"replicas": replicas, "pod": pod}})
    }

    fn c(name: &str, resources: serde_json::Value) -> serde_json::Value {
        json!({"name": name, "image": "alpine:3.22", "resources": resources})
    }

    #[test]
    fn pods_request_their_effective_request() {
        let p = program(
            "a",
            json!([set(
                "w",
                2,
                json!({
                    "init": [c("big-init", json!({"cpu_request_millis": 900, "memory_request_bytes": 10485760})),
                             c("small-init", json!({"cpu_request_millis": 100}))],
                    "sidecars": [c("side", json!({"cpu_request_millis": 200, "memory_limit_bytes": 33554432}))],
                    "containers": [c("main", json!({"cpu_limit_millis": 300})), c("bare", json!({}))]
                })
            )]),
        );
        let Spec::ReplicaSet(s) = &p.resources[0].spec else {
            panic!()
        };
        // CPU: max(900, 200 + 300 from the limit) = 900; memory: max(10 MiB, 32 MiB from the limit) = 32 MiB.
        assert_eq!(
            pod(&s.pod),
            Totals {
                cpu_millis: 900,
                memory_bytes: 33554432
            }
        );
        assert_eq!(
            super::program(&p),
            Totals {
                cpu_millis: 1800,
                memory_bytes: 67108864
            }
        );
    }

    #[test]
    fn revisions_that_do_not_fit_are_refused_with_figures() {
        let allocatable = allocatable(
            Capacity {
                cpu_millis: 4000,
                memory_bytes: 8 << 30,
            },
            Totals {
                cpu_millis: 250,
                memory_bytes: 512 << 20,
            },
        );
        assert_eq!(
            allocatable,
            Totals {
                cpu_millis: 3750,
                memory_bytes: (8 << 30) - (512 << 20)
            }
        );
        let a = program(
            "a",
            json!([set(
                "web",
                2,
                json!({"containers": [c("w", json!({"cpu_request_millis": 1000}))]})
            )]),
        );
        let b = program(
            "b",
            json!([set(
                "api",
                2,
                json!({"containers": [c("a", json!({"cpu_request_millis": 1000}))]})
            )]),
        );
        let others = BTreeMap::from([("a".to_string(), a.clone())]);
        assert!(
            check("a", &a, &others, allocatable).is_ok(),
            "a deployment replaces its own earlier revision"
        );
        let refused = check("b", &b, &others, allocatable).unwrap_err();
        assert_eq!(
            refused,
            "requests exceed the host's allocatable capacity: CPU: 4000m requested of 3750m allocatable (deployment \"b\" 2000m: api 2 × 1000m; other deployments 2000m). Lower requests or replicas, or remove other deployments."
        );
        let smaller = program(
            "a",
            json!([set(
                "web",
                1,
                json!({"containers": [c("w", json!({"cpu_request_millis": 1000}))]})
            )]),
        );
        let others = BTreeMap::from([("a".to_string(), smaller)]);
        assert!(check("b", &b, &others, allocatable).is_ok());
        let hungry = program(
            "m",
            json!([set(
                "db",
                1,
                json!({"containers": [c("pg", json!({"memory_request_bytes": 9663676416u64}))]})
            )]),
        );
        let refused = check("m", &hungry, &BTreeMap::new(), allocatable).unwrap_err();
        assert!(
            refused.contains("memory: 9216 MiB requested of 7680 MiB allocatable")
                && refused.contains("db 1 × 9216 MiB"),
            "{refused}"
        );
    }
}
