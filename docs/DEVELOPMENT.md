# Development

Rezn currently reconciles Docker containers through Orqos. Kubernetes reconciliation is deferred.

## Build and checks

Use Rust and the system OpenSSL development libraries. On Ubuntu:

```bash
sudo apt install build-essential pkg-config libssl-dev
cargo build --locked --workspace
cargo fmt --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets
```

Push/PR CI runs formatting, build, and ordinary tests with Rust 1.99.0. The live Docker test is explicitly invoked. Published releases retain artifact builds.

## Run locally

With Orqos checked out alongside Rezn, start a Docker-only Orqos using the intended Docker Desktop socket:

```bash
ORQOS_BACKENDS=docker \
DOCKER_SOCKET="$(docker context inspect desktop-linux --format '{{.Endpoints.docker.Host}}')" \
cargo run --locked --manifest-path ../orqos/Cargo.toml
```

In another terminal, run Rezn from its repository root:

```bash
ORQOS_API_URL=http://127.0.0.1:3000 \
STATS_WS_URL=ws://127.0.0.1:3000/stats/ws \
cargo run --locked -p rezn
```

Keep `ORQOS_API_URL` at the server root. Lifecycle requests use `/docker/containers`, `/docker/containers/{id}/stop`, and `/docker/containers/{id}/remove`. Stats still use `/stats/ws`.

Pull every desired image into the same engine before applying workloads. Orqos reports missing images as 404, conflicting names as 409, and unavailable enabled backends as 503. Rezn includes response details in errors; failed list requests prevent mutations for that reconciliation pass. Once requests succeed, periodic reconciliation resumes.

## Docker smoke test

With Docker-only Orqos running as above:

```bash
docker --context desktop-linux pull nginx:alpine
ORQOS_API_URL=http://127.0.0.1:3000 \
cargo test --locked -p rezn docker_reconciliation_smoke -- --ignored --nocapture
```

The test requires a loopback endpoint, uses a temporary state database and unique ownership labels, checks replicas `1 → 2 → 1 → 0`, and verifies repeated reconciliation preserves container identities. Cleanup includes stopped test containers and runs after failed checks. The test exercises the reconciliation loop directly; signed `/apply` handling is separate.
