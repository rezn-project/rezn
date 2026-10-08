# Signature limits and follow-up

Rezn verifies Ed25519 over JCS-canonicalized submitted `program` values, preserving all program keys through verification and rejecting unsupported fields afterward. Accepted state retains the complete signed program envelope; validation never removes fields to rescue a signature or make an unsupported example executable.

The public key comes from the caller. Any caller who can reach the local API can submit their own signed program. The deployment name is outside the signature, and there is no signed revision or replay protection. The controller's stored revision is a local observation fence, not an authenticated revision contract. Ownership labels isolate cooperative local controllers, but labels can be forged by anyone with Docker access. These mechanisms do not provide an authorization boundary.

Before expanding beyond local development, define trusted signer configuration and a signature contract binding deployment identity and monotonic revisions (including deletion/empty intent). Then enforce authorization and replay checks atomically with stored intent. These changes are deliberately deferred in this MVP.

## DSL canonicalization

At sibling `rezn-dsl` commit `09f0bb8`, `lib/sign.ml` canonicalized program JSON
before signing, while `lib/verify.ml` verified ordinary `Yojson.Safe.to_string`
bytes. The compiler CLI canonicalized its final output, which hid the mismatch
in that workflow. Direct library signer output and reordered program objects
could fail verification despite containing the signed values.

The shared DSL `verify_bundle` function now canonicalizes the complete program
with the same `Jcs_bindings.canonicalize` helper as the signer. This fixes both
the verifier CLI and the server without changing the envelope or signature
algorithm. Canonicalization errors still fail verification. Tests reproduced
the old failure before the patch and pass afterward, including direct library
round trips, nested object key order, formatting, equivalent `1`/`1.0` numbers,
empty programs, tampered values/array order, wrong keys/signatures, and a
canonicalization error. The real compiler, verifier CLI and Unix-socket server
were also exercised with valid and tampered bundles.

A shared fixture is kept as identical copies in
`rezn-dsl/test/fixtures/signatures/` and
`rezn/rezn-runtime/tests/fixtures/signatures/`. An independent OpenSSL-generated
Ed25519 signature covers the exact bytes in `program.canonical.json`; both
verifiers check the canonical bytes and signature. The private test key is not
included. Fixture notes explain how to keep the copies synchronized.

From the DSL repository root, with its existing dependencies and JCS library:

```bash
REZNJCS_LIB_PATH=/absolute/path/libreznjcs.so opam exec -- dune build
REZNJCS_LIB_PATH=/absolute/path/libreznjcs.so opam exec -- dune runtest
```

From this repository, `cargo test --locked --workspace` runs the Rust fixture
check alongside the runtime regressions. The signer trust, deployment binding
and replay limitations above remain separate work.

Never delete `secure`, `env`, service, volume or enum instructions from already
signed bundles to make them executable: that changes the signed program and
must invalidate its signature. Compile/sign a supported pod-only program instead.
