# Rezn

**Rezn** is meant to be a Gleam-based control loop for container orchestration.  
It's not a PaaS. It's not Kubernetes. And it's not ready.

This is not a POC. It’s a starting point.

---

## Goals (WIP)

- Define desired state as Gleam code
- Compare against actual state (via Docker API)
- Reconcile the difference
- Run as a supervised process on the BEAM

---

## Not Yet Implemented

- Session lifecycle
- State storage
- API layer
- CLI
- Anything useful

---

## Dependencies

- Docker daemon
- Erlang/OTP
- Gleam

---

## You should not use this

This is scaffolding.  
Nothing works yet.  
That’s the point.
 
