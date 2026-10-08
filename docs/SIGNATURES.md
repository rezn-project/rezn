# Signature limits and follow-up

Rezn verifies Ed25519 over JCS-canonicalized submitted `program` values, preserving all program keys through verification and rejecting unsupported fields afterward. Accepted state retains the complete signed program envelope; validation never removes fields to rescue a signature or make an unsupported example executable.

The public key comes from the caller. Any caller who can reach the local API can submit their own signed program. The deployment name is outside the signature, and there is no signed revision or replay protection. The controller's stored revision is a local observation fence, not an authenticated revision contract. Ownership labels isolate cooperative local controllers, but labels can be forged by anyone with Docker access. These mechanisms do not provide an authorization boundary.

Before expanding beyond local development, define trusted signer configuration and a signature contract binding deployment identity and monotonic revisions (including deletion/empty intent). Then enforce authorization and replay checks atomically with stored intent. These changes are deliberately deferred in this MVP.

## Separate DSL inspection

Read-only inspection of sibling `rezn-dsl` at commit `09f0bb8` found a canonicalization discrepancy:

- `lib/sign.ml`, `generate_signed_bundle`, calls `Jcs_bindings.canonicalize` before signing the program.
- `lib/verify.ml`, `verify_bundle`, serializes the program with `Yojson.Safe.to_string` and verifies those bytes without canonicalizing them.
- `reznc/main.ml` canonicalizes the final bundle, which can make the ordinary CLI output's program ordering match the signed bytes. That does not fix the verifier's sensitivity to another valid serialization of the same program.

Consequently, reordering program object keys or using a different valid string/number encoding can make the DSL verifier reject a canonically equivalent signed program. Rezn verifies the canonical representation and tests reordered supported signed input. This finding is based on source inspection; the DSL verifier binary was not available for an execution test. No files in that repository were changed.

The follow-up should canonicalize program values in the DSL verifier and add shared canonicalization/signature fixtures for compiler, verifier and runtime. Do not work around it by deleting `secure`, `env`, service, volume or enum instructions from already signed bundles: that changes the signed program and must invalidate its signature. Compile/sign an explicitly supported pod-only program instead.
