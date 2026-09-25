# TxProof

TxProof is a Rust test harness for **bounded counterexample search** in money-moving backend workflows. It generates state-valid failure campaigns, runs them against an explicitly configured disposable application and PostgreSQL database, checks invariants, and records decisions for deterministic replay and shrinking.

The currently integrated target is a repository-owned synthetic payment reference application with a fixture-style PaymentIntent provider. TxProof does not prove arbitrary payment systems correct, exercise a live Stripe account, or establish customer production use.

## A two-minute technical tour

Start with the [commit-then-close failure walkthrough](docs/reviewer-proof.md). It shows one provider ambiguity, the duplicate object it can cause, the invariant that detects it, and the corrected retry behavior. Every result in that walkthrough points to an executable test and implementation source.

| Question | Inspect |
| --- | --- |
| How are valid, bounded campaigns planned? | [`crates/tiv-core/src/plan.rs`](crates/tiv-core/src/plan.rs) |
| What happens when the provider commits and closes the connection? | [`crates/tiv-cli/tests/configured_run.rs`](crates/tiv-cli/tests/configured_run.rs) |
| What is the read-only invariant boundary? | [`docs/invariant-snapshot-runner.md`](docs/invariant-snapshot-runner.md) |
| What prevents a reset of the wrong database? | [`crates/tiv-runtime/src/postgres/safety.rs`](crates/tiv-runtime/src/postgres/safety.rs) and [`docs/baseline.md`](docs/baseline.md) |
| How is a finished artifact checked without running customer code? | `tiv inspect PATH`, documented in the [inspection contract](docs/rust-backend-architecture-plan.md#implemented-complete-artifact-inspection-contract) |
| What runs in public CI? | [Rust verification workflow](.github/workflows/rust.yml) |

## Safe first commands

From this repository root with the Rust toolchain specified in [`rust-toolchain.toml`](rust-toolchain.toml):

```sh
cargo test --workspace --all-targets
cargo run -p tiv-cli -- --help
cargo test -p tiv-cli --test configured_run -- --list
```

The first command runs the ordinary workspace suite; Docker-backed reference-app tests are marked `ignored`. The last command lists those integration cases without executing them. This is a useful source-level tour before preparing an isolated reference stack.

For a complete local campaign, follow [`docs/reviewer-proof.md`](docs/reviewer-proof.md) and the isolated [`reference-app-spike` CI job](.github/workflows/rust.yml). That path uses synthetic credentials and a disposable Compose project. It performs database resets inside that isolated project. Do not point its configuration at shared, production, or customer data.

## How a run is interpreted

`tiv run` executes a configured bounded campaign and emits a path to a private evidence artifact. `tiv inspect PATH` is read-only: it checks the complete artifact and returns a small verification receipt without starting services or printing the evidence payload. `tiv replay configured` reruns one verified violating case against a freshly attested baseline, and `tiv shrink configured` searches for a smaller reproducing history under fixed budgets. The CLI help and [`docs/rust-backend-architecture-plan.md`](docs/rust-backend-architecture-plan.md) show the exact arguments and compatibility checks.

The five configured SQL invariants are evaluated inside one `READ ONLY REPEATABLE READ` PostgreSQL transaction using a constrained role. A failing query is a bounded counterexample for the configured application, fixture, schedule, and invariant contract. It is not a formal proof over all executions.

## Project boundaries

- The planner caps v1 campaigns at 500 cases and 40 actions per case.
- Destructive baseline/reset operations require a disposable database marker and an exact acknowledgement tied to the server, database, owner, project, and application role. The identity is checked again before a single-use mutation permit.
- Reference fixtures and local credentials are synthetic. Finalized artifacts are private and verified before inspection or replay; an illustrative walkthrough in this README does not substitute for a complete artifact.
- The repository does not yet declare a reuse license. Public source visibility alone does not grant a license.

For setup details, read [`docs/init.md`](docs/init.md), [`docs/config-and-doctor.md`](docs/config-and-doctor.md), and [`docs/baseline.md`](docs/baseline.md) in that order. The generated `tiv init` templates fail closed until their application-specific placeholders are replaced.
