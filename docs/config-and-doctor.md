# Typed config and `tiv doctor`

This slice establishes TxProof's version-one TOML contract and a deliberately
non-mutating readiness preflight. It does not start services, connect to a
database, create a baseline, authorize reset, or execute customer code.

## Run the preflight

`tiv.toml` stores environment-variable names, never secret values. Resolve the
three required local-test values in the process environment, then run:

```text
tiv doctor --config path/to/tiv.toml
```

The complete contract specimen used by the reference stack is
`tests/golden/doctor-project/tiv.toml`. Its input files are intentionally inert
fixtures. New repositories can create the editable fail-closed skeleton with
`tiv init`; see `docs/init.md`.

The preflight performs these bounded checks:

1. Deserialize the versioned TOML with unknown fields denied.
2. Canonicalize every input path and require it to stay inside the Git
   repository.
3. Enforce local-disposable mode, serial execution, run/time/database limits,
   the supported Stripe fixture protocol, and exactly five invariants.
4. Bind future mutating commands to one explicit lowercase Compose
   `project_name`; service names alone are not an ownership boundary.
5. Resolve the referenced database URLs and webhook secret from environment
   names while keeping their values in non-serializable types.
6. Invoke only `docker --host unix:///var/run/docker.sock compose ... version
   --short` and `docker --host unix:///var/run/docker.sock compose ... config
   --format json`, with exact project/file arguments, timeouts, and output
   limits.
7. Require the configured application, PostgreSQL, Stripe fixture, and worker
   services; reject live Stripe material and public database targets; redact
   all Compose environment values before hashing the resolved graph.

Ambient Docker context and TLS variables are removed from both probes. A
successful report has `status: "ready"` and `mutation_authorized: false`.
"Ready" means the typed configuration and resolved Compose graph passed this
preflight; it does not mean the services are running or healthy.

## Output and exit contract

Standard output is one allowlisted JSON document. It includes normalized
budgets and URLs, environment-variable names, the isolated probe project,
Compose version and service names, and a hash of the resolved redacted Compose
document. It excludes database URLs, passwords, tokens, webhook secrets, and
raw Compose environment values.

- `0`: the preflight passed.
- `2`: invalid configuration or safety preflight failure.
- `3`: Docker/Compose infrastructure or setup failure.

## Pinned JSON Schema

The development schema is checked in at
`schemas/tiv-config-v1.schema.json`. Runtime correctness still comes from Rust
deserialization and semantic validation. The contract test fails when the
generated schema drifts; inspect it directly with:

```text
cargo run --quiet -p tiv-runtime --example config_schema
```
