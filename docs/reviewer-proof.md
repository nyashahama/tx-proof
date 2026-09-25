# One failure: a committed payment with a lost response

This walkthrough explains an executable **synthetic regression**, not a retained complete TxProof run artifact. The code and historical CI linked below are the evidence for each result. A local `tiv inspect` command requires a complete artifact produced by an isolated run.

## The failure boundary

1. The caller sends a create-payment request with an idempotency key.
2. The fixture provider commits the PaymentIntent but closes the connection before the caller receives a response (`commit_then_close`).
3. The faulty reference-app mode retries with a changed idempotency key. The provider now sees a new operation and creates a second object.
4. TxProof's configured `provider-object-unique` invariant returns one witness. The test asserts two provider objects, a violating verdict, and CLI exit code `10`.
5. In the repaired reference-app mode, the retry keeps the same key. The test asserts one provider object, all five configured invariants holding, and exit code `0`.

The implementation and assertions are in [`crates/tiv-cli/tests/configured_run.rs`](../crates/tiv-cli/tests/configured_run.rs): `row_three_changed_key_fault_violates_and_same_key_repair_holds`. The fault-mode values are `faulty_changed_key` and `repaired_same_key`. [`tests/configured-run-project/tiv.toml`](../tests/configured-run-project/tiv.toml) declares the synthetic reference stack and five SQL invariants.

| Case | Recorded test expectation | Meaning |
| --- | --- | --- |
| Changed-key retry | `provider_object_count = 2`; `provider-object-unique` violated with one witness; exit `10` | The same logical checkout can produce two provider objects. |
| Same-key retry | `provider_object_count = 1`; all five invariant verdicts `held`; exit `0` | That bounded case does not reproduce the duplicate. |

These values summarize assertions in the checked-in regression. They are not copied from a current run, do not constitute independent production measurements, and cannot be passed to `tiv inspect` as an artifact.

## Reproduce and inspect safely

First inspect the case and verify the available commands without creating or resetting databases:

```sh
cargo test -p tiv-cli --test configured_run -- --list
cargo run -p tiv-cli -- --help
docker compose --project-name tiv-reference-app-spike --file spike/reference-app.compose.yaml config --quiet
```

The named regression is marked `ignored` because it starts and resets the **isolated reference-app Compose stack**. The [CI reference-app job](../.github/workflows/rust.yml) is the canonical executable setup: it validates the Compose topology, starts only that project, uses synthetic fixture credentials and the dedicated loopback PostgreSQL port, runs the ignored test, and removes that project's containers and volumes afterward. Its latest successful public run at the time of this walkthrough is [2026-08-25, run 32828881852](https://github.com/nyashahama/tx-proof/actions/runs/32828881852). The workflow's retained logs have a limited lifetime.

If you reproduce the run locally, use a disposable machine or namespace, check that the named project and ports are free, and follow the workflow's exact setup. The ignored test deletes only its configured private `.tiv` artifacts and disposable reference databases as part of its fixture lifecycle. A successful local test cleans up those artifacts, so copy **only a reviewed, secret-free report excerpt** if you need to retain a public example. Never publish raw database credentials, webhook secrets, or a customer's artifact.

For a retained complete local artifact, the CLI returns `artifact_path` after `tiv run`. Inspection is deliberately read-only:

```sh
cargo run -p tiv-cli -- inspect <artifact-path>
```

The receipt establishes checksum and compatibility verification. The [inspection contract](rust-backend-architecture-plan.md#implemented-complete-artifact-inspection-contract) explains the exact boundary. Replay and shrink require a compatible configured artifact and a fresh authorized test baseline; the commands are described in [`docs/rust-backend-architecture-plan.md`](rust-backend-architecture-plan.md).

## Why this is interesting

The bug is not a failed HTTP response by itself. The ambiguity is that the provider may have committed while the caller has no receipt. A changed retry identity can create another object, so the invariant is expressed against durable provider/application state. TxProof makes the schedule, witness, and replay authority explicit, then checks the corrected mode against the same bounded failure family.

The evidence is limited to the repository-owned reference app and fixture provider. It supports reasoning about failure and recovery; it does not show live Stripe behavior, a customer deployment, or a proof that every payment history is safe.
