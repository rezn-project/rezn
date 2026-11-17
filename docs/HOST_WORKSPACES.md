# RawPair native host workspace boundary

Updated: 2026-11-17. RawPair's native Linux workspace MVP was delivered on 2026-10-11 in RawPair and Orqos. Rezn was not changed for it.

Current documentation, assuming sibling checkouts under `~/dev`:

- [RawPair delivery handoff](../../rawpair/docs/HANDOFF.md) and [package handoff](../../rawpair/docs/PACKAGE_HANDOFF.md): implemented behavior, checks run, unverified live paths and limits.
- [RawPair package guide](../../rawpair/docs/PACKAGES.md) and [native source setup](../../rawpair/docs/NATIVE_SETUP.md): operator installation.
- [Orqos host API](../../orqos/docs/HOST_API.md): the implemented `/host` contract.
- [RawPair goal](../../rawpair/goal.txt), [product brief](../../rawpair/docs/HOST_MVP.md) and [Orqos design brief](../../orqos/docs/HOST_BACKEND.md): the original scope.

## Rezn scope

Rezn remains a local Docker controller using Orqos's Docker API. Its supported signed program is container-oriented: `pod` instructions with image, replicas and ports. Native development workspaces are managed by RawPair through Orqos's separate, token-protected `/host` API; they do not use Rezn reconciliation or signed DSL intent.

Orqos kept its Docker routes and semantics consumed by Rezn. Native mode (`ORQOS_BACKENDS=host`) does not mount the Docker routes, so Rezn needs an Orqos instance running with the `docker` or `both` backend.

## Possible later work

If Rezn later manages native host workloads declaratively, define an explicit native workload model, execution/ownership contract and reconciliation policy against the Orqos host API. Do not reinterpret a container image as a host shell command or silently drop signed fields. A DSL change would belong to that separate effort.

Trusted signers, deployment binding and replay protection remain separate prerequisites for expanding Rezn beyond its current local-development signature contract; see [SIGNATURES.md](SIGNATURES.md). They were not dependencies of RawPair's native workspace MVP.
