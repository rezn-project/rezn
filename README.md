# Rezn
**The Typed, Secure & Sane Way to Declare Infrastructure**  

---

## Why another tool?

Because every time a production cluster topples over due to a missing space in a `deployment.yaml`, an SRE's soul leaves their body.  
Kubernetes was a miracle for scheduling, **not** for expressing intent.
Rezn is a clean break:

```
pod "api" {
  image     = "my-app:latest"
  replicas  = 3
  ports     = [4000]
}
````

That single block:

* is **strongly-typed** (strings aren't accidentally ports, integers aren't accidentally images)
* supports **logic** (‐ `replicas = env("ENV") == "prod" ? 5 : 1`)
* is wrapped in **contracts / invariants** (*Design-by-Contract*)
* is compiled into a **signed, tamper-proof IR**
* is fed to a **reconciliation loop** that makes reality match intent, every few seconds, no YAML in sight.

---

## What Rezn *is*

| Layer                | Responsibility                                                                                                  |
| -------------------- | --------------------------------------------------------------------------------------------------------------- |
| **Compiler**    | Parses the DSL, type-checks, enforces contracts, emits a signed Intermediate Representation (IR).               |
| **Controller**  | Loads the IR, continually diffs **desired state ↔ actual state**, produces a minimal action plan.               |
| **Executor** | Runs on each host, **guardian of the Docker / Podman socket**. Executes only signed commands coming from Reznd. |

All communication is authenticated, integrity-checked, and replay-proof.
Your cluster obeys only **cryptographically verifiable truth**.

---

## What Rezn *is not*

* **Not** a template engine.
* **Not** a giant YAML linter.
* **Not** "Helm-but-compile-time."
* **Not** married to any single programming language; tooling is pluggable and implementation-agnostic.

---

## Feature Highlights

| ✅                        | Feature                                                                                          | Why it matters |
| ------------------------ | ------------------------------------------------------------------------------------------------ | -------------- |
| ✅ | **Typed DSL**            | Declare infra with first-class types instead of stringly chaos.                                  |                |
| ✅ | **Design-by-Contract**   | Attach invariants and pre/post-conditions to every resource. Compiler refuses invalid states.    |                |
| ✅ | **Real logic**           | Conditionals, variables, simple expressions  *inside* the config, no templating monkey-patches. |                |
| ✅ | **Signed IR**            | Desired state is cryptographically signed; executors reject anything forged or stale.            |                |
| ✅ | **Solid reconciliation** | A tight loop continually converges actual ↔ desired without guessing.                            |                |
| ✅ | **No runtime surprises** | If it compiles, it deploys. If it violates a contract, it never leaves your laptop.              |                |
| ✅ | **Tiny footprint**       | One small, static binary per component; no JVMs, no 160 MB Node runtimes.                        |                |

---

## Workflow at a glance

1. **Write** `infra.rezn`
2. `rezn build infra.rezn` → `state.rir` (signed)
3. `reznd apply state.rir` (local or remote)
4. **Done** – Executor nodes reconcile until reality matches the plan.

Need a diff? `reznd diff state.rir`.
Need rollback? Keep signed snapshots, they'’'re immutable.

---

## Why “**Forget YAML**” is more than a slogan

YAML is a serialization format, not a source of truth.
And certainly not a programming model.

* **Indentation is not structure.**
* **Schemas are not contracts.**
* **Validation at runtime is not safety.**

We believe infrastructure should:

* **Fail fast, at compile time.**
* Be defined in a language with **types**, **logic**, and **invariants**.
* Be signed and auditable.

Rezn is not another YAML tool.
It’s an **explicit, verifiable compiler** for infrastructure intent.

---

## Current Status & Roadmap

| Phase                            | Status                 |
| -------------------------------- | ---------------------- |
| DSL grammar & reference compiler | Concept phase          |
| Signed IR format (`.rir`)        | Prototype (Ed25519  )  |
| Reznd diff / apply engine        | Concept phase          |
| Executor daemon                  | In progress            |
| TUI / dashboards                 | Planned                |
| k8s YAML importer                | Planned                |

---

## Contributing

* Love type systems? Help extend the DSL.
* Into distributed systems? Harden the reconciliation loop.
* Security nerd? Review the signing & verification pipeline.
* Hate YAML as much as we do? Open an issue and let's plot together.

> We're intentionally vague about implementation languages; because **design > syntax**.
> What matters is the contract between *intent* and *reality*, and that contract is written in Rezn.

---

### Help us banish YAML
