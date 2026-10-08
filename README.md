# Rezn

Rezn is a local Docker controller that accepts signed desired intent and converges containers through the existing Orqos Docker API.

The executable subset is deliberately small: named `pod` instructions with an image, replica count and TCP container ports. Images must already exist in the selected Docker engine. Applying intent replaces one deployment's complete program; an empty signed program removes its workloads. Replica count zero is valid.

```json
[{"kind":"pod","name":"web","fields":{"image":"nginx:alpine","replicas":1,"ports":[80]}}]
```

The [supported signed example](examples/test.ir.json) contains only executable fields. With Docker-only Orqos and Rezn running locally:

```bash
cargo run --locked -p reznctl -- http://127.0.0.1:4000/apply demo examples/test.ir.json
curl -s http://127.0.0.1:4000/state
curl -s http://127.0.0.1:4000/status
```

`/apply` returns HTTP 202 when intent is stored, before deployment completes. `/state` reports desired intent; `/status` reports observed replicas, configuration identities, container states, allocated host ports, timestamps and errors. Reconciliation runs after apply and periodically, replacing changed or stopped containers and cleaning up removed workloads. Unchanged reapplication preserves container IDs. Replacement permits downtime and new host ports.

Each database has a persistent ownership identity. Rezn checks ownership labels before mutating a container and never adopts containers by name. The state format is new: legacy databases require an explicitly chosen fresh database, with old resources handled separately. See [development, status and state-format documentation](docs/DEVELOPMENT.md).

Signatures verify canonical program integrity against the **caller-supplied public key**. They do not establish signer trust, bind the deployment name, or prevent replay. This is a local development MVP. Kubernetes, Podman certification, rolling updates, automatic image pulls and metrics/UI improvements are deferred.

Build checks and the reproducible live Docker acceptance command are in the [development guide](docs/DEVELOPMENT.md). The separate DSL compiler's signing/verifying discrepancy is recorded in [signature follow-up notes](docs/SIGNATURES.md).
