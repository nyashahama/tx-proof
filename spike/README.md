# PostgreSQL truth spike

This disposable stack proves one generated template reset and one five-query,
read-only, repeatable-read invariant snapshot. It is synthetic test data only.

The stack is intentionally isolated:

- Compose project: `tiv-truth-spike-postgres`
- PostgreSQL host binding: `127.0.0.1:15432`
- Image: `postgres:18.4-bookworm`
- Network: project-scoped bridge; the only host publication is loopback-only
- Generated databases: `tiv_base_<run>` and `tiv_case_<run>`
- Cleanup: the exact Compose project plus its project-owned volume

Render and inspect configuration before starting it:

```sh
docker compose \
  --project-name tiv-truth-spike-postgres \
  --project-directory "$PWD" \
  --file spike/postgres.compose.yaml \
  config
```

The ignored integration test is run only while that exact isolated stack is
healthy:

```sh
TIV_POSTGRES_TEST_PORT=15432 \
  cargo test -p tiv-runtime postgres::spike::tests:: -- \
    --ignored --test-threads=1 \
    --skip real_reference_app_replays_commit_close_with_the_same_failure_identity
```

Pull requests and `main` run the same boundary in
`.github/workflows/rust.yml`. The job retains the Compose service-configuration
hash and bounded test log for 14 days, without publishing the resolved
environment, then removes only the exact truth-spike project and its
project-owned volume even when the test fails.

Every synthetic persistent write re-observes the complete marked case
identity immediately before mutation. The live test also carries a stale
pre-reset identity across a template clone and proves that it is rejected
without changing the clean case database.

The operation-level `provider-object-unique` invariant is a truth-spike
interpretation, not yet a final customer invariant template. It reports an
operation that has more than one provider PaymentIntent ID. The other four
queries are explicit no-ops in this slice so the exact five-query transaction
shape is exercised without claiming unimplemented product semantics.

The PostgreSQL-only stack does not mark its sole bridge `internal: true`.
Docker 29 accepted the declared port binding but did not install a runtime
publication for a container attached only to a gateway-less internal network.
The multi-service topology below uses a separate internal data network while
retaining loopback-capable host networks for control and evidence collection.

## Real reference application spike

`reference-app.compose.yaml` adds the next Phase 0 boundary: a real synthetic
application and a long-lived Stripe-shaped fixture around the same disposable
PostgreSQL safety layer.

The topology keeps four separate paths:

- an internal data network shared by the application, fixture data listener,
  and PostgreSQL;
- a fixture-control network published only at `127.0.0.1:12112`;
- an application-ingress network published only at `127.0.0.1:18080`;
- a PostgreSQL host network published only at `127.0.0.1:15432`.

The application is not attached to the fixture-control network. The acceptance
test asks the application to probe `stripe-fixture:12112` on their shared data
network and requires that connection to fail. The fixture data listener uses a
different, data-network-only alias so multi-network DNS cannot bind it to the
control address.

Start the exact isolated project:

```sh
docker compose \
  --project-name tiv-reference-app-spike \
  --project-directory "$PWD" \
  --file spike/reference-app.compose.yaml \
  config --quiet

docker compose \
  --project-name tiv-reference-app-spike \
  --project-directory "$PWD" \
  --file spike/reference-app.compose.yaml \
  up --detach --build --wait
```

### Reference fault modes and repaired controls

The reference application selects its retry behavior once at process startup
through `TIV_REFERENCE_APP_RETRY_KEY_MODE`. The only accepted values are
`faulty_changed_key` and `repaired_same_key`; the Compose default is the faulty
mode. Caller retry behavior is independently startup-only through
`TIV_REFERENCE_APP_CALLER_RETRY_MODE`, with exact values
`faulty_per_request` and `repaired_recover_operation`; the faulty mode is the
default. Reconciliation behavior is startup-only through
`TIV_REFERENCE_APP_RECONCILIATION_MODE`, with exact values
`faulty_webhook_only` and `repaired_provider_reconcile`; the webhook-only fault
is the default. Webhook business-effect behavior is likewise startup-only through
`TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE`, with exact values
`faulty_duplicate_effect` and `repaired_deduplicate`; the repaired mode is the
default. Ledger posting behavior is startup-only through
`TIV_REFERENCE_APP_LEDGER_MODE`, with exact values
`faulty_one_sided_duplicate` and `repaired_balanced_once`; the repaired mode is
the default. There is no request header or route parameter that can switch any
behavior inside a run. `/health` reports all five non-secret modes, and the
direct `reference-app-evidence` command additionally attests the exact
faulty-key, faulty-caller, faulty-reconciliation, repaired-effect,
repaired-ledger mode set before it labels a changed-key counterexample. The duplicate-effect and one-sided-ledger
faults are mutually exclusive; selecting both fails process startup rather
than reporting a mode whose behavior is hidden by branch precedence.

The resolved Compose configuration, and therefore its configuration hash,
includes all five modes. A command inspecting a non-default stack must receive
the same environment values; evidence from one mode set is intentionally not
compatible with a stack resolved under another set.

The row-one pair uses campaign seed `1792`, which compiles one provider object,
one immutable event, one original delivery, and one duplicate delivery without
a process kill. Faulty mode records two effect applications and produces one
`webhook-effect-at-most-once` witness. Repaired mode uses an atomic processed
event key, records one effect, and all five configured invariants hold. The
faulty artifact also reproduces the same identity in 3/3 fresh baselines; a
three-candidate bounded shrink rejects removal of the causal duplicate, accepts
a smaller seven-action trace, and the minimized trace reproduces 3/3:

```sh
CARGO_INCREMENTAL=0 cargo test -p tiv-cli --test configured_run \
  row_one_duplicate_webhook_fault_violates_and_deduplicated_repair_holds \
  -- --ignored --exact --nocapture
```

The row-six pair reuses seed `1792` with repaired retry and effect modes. The
first accepted effect writes one delivery-bound journal header with a balanced
`processor_clearing` debit and `order_payment_liability` credit. Faulty ledger
mode writes a second header with only the debit when the same event is
delivered again, producing exactly one `balanced-ledger` witness while the
other four invariants hold. The faulty artifact and seven-action minimized
authority reproduce 3/3. Source, replay, shrink, and minimized artifacts retain
the exact bounded ledger witness with its BLAKE3 digest. Repaired ledger mode
records one header, two postings, and all five invariants hold:

```sh
CARGO_INCREMENTAL=0 cargo test -p tiv-cli --test configured_run \
  row_six_one_sided_ledger_fault_violates_and_balanced_repair_holds \
  -- --ignored --exact --nocapture
```

The paired row-three acceptance test owns both mode changes, executes campaign
seed `69` against a fresh baseline in each mode, verifies both finalized
artifacts, requires a verified default-mode restore on normal completion, and
makes a best-effort default-mode restore if an assertion unwinds the test. CI
also recreates and attests the default mode set in an unconditional follow-up step
before any later reference evidence can run:

```sh
CARGO_INCREMENTAL=0 cargo test -p tiv-cli --test configured_run \
  row_three_changed_key_fault_violates_and_same_key_repair_holds \
  -- --ignored --exact --nocapture
```

The compiled trace is identical in both executions: one checkout with
`commit_then_close` followed by `normal`. The faulty app commits two provider
objects and violates `provider-object-unique`; the repaired app reuses one
provider object, aliases both attempt outputs to that identity, and all five
configured invariants hold. This is a bounded paired regression for reference
fault row 3, not a proof over arbitrary schedules or customer applications.

The row-four pair uses campaign seed `422`. The compiled trace completes one
checkout, observes its successful application response, SIGKILLs the app before
that logical response is acknowledged to the caller, restarts it, and issues a
new caller business request. The faulty caller mode creates one provider object
per request, leaving two provider objects and two distinct local payment rows;
only `provider-object-unique` fails. The repaired caller mode first performs the
fixture's bounded operation-metadata lookup, accepts exactly one matching
PaymentIntent, and persists the recovered provider/local pair without issuing
another create. It therefore leaves one provider object, one local payment row,
and all five invariants held. Source replay and the minimized authority are
stable 3/3; shrink rejects candidates that remove either the response-observed
crash or the caller retry:

```sh
CARGO_INCREMENTAL=0 cargo test -p tiv-cli --test configured_run \
  row_four_lost_response_caller_retry_fault_violates_and_recovery_holds \
  -- --ignored --exact --nocapture
```

This is a bounded synthetic recovery contract. The operation lookup is a
fixture-only Stripe-shaped endpoint, not a claim that the public Stripe API
supports arbitrary PaymentIntent lookup by metadata.

The row-five pair uses campaign seed `359`. It creates one PaymentIntent,
confirms it, generates one immutable success event, and drops that event before
delivery. A checkout-scoped watchdog holds one real `tiv_app` database session
until the payment is settled or the declared five-second reconciliation horizon
closes, so the quiescence predicate cannot classify the fault early. In
webhook-only mode the local payment remains pending and exactly
`paid-order-amount-conservation` fails after the horizon. Repaired mode polls
only the exact validated provider object, updates the matching local payment to
succeeded through the existing least-privilege status grant, and releases the
session; all five invariants then hold. Source replay and the minimized
authority are stable 3/3, and shrink rejects removal of the causal drop:

```sh
CARGO_INCREMENTAL=0 cargo test -p tiv-cli --test configured_run \
  row_five_dropped_success_fault_violates_and_reconciliation_converges \
  -- --ignored --exact --nocapture
```

The five-second window is an explicit compressed reference horizon, not a
universal correctness deadline for customer systems.

Then run the real application path:

```sh
TIV_FIXTURE_CONTROL_TOKEN=run-scoped-control-token \
TIV_POSTGRES_ADMIN_PASSWORD=tiv-local-only-password \
TIV_POSTGRES_APPLICATION_PASSWORD=tiv-app-local-only-password \
  cargo run --quiet -p tiv-cli --bin tiv -- \
    replay reference-app-evidence \
    --trace spike/compiled-trace-v1.json \
    --postgres-port 15432 \
    --reference-app-url http://127.0.0.1:18080 \
    --fixture-control-url http://127.0.0.1:12112
```

Run the compiled serial provider/webhook case with a new durable journal path:

```sh
TIV_FIXTURE_CONTROL_TOKEN=run-scoped-control-token \
TIV_POSTGRES_ADMIN_PASSWORD=tiv-local-only-password \
TIV_POSTGRES_APPLICATION_PASSWORD=tiv-app-local-only-password \
  cargo run --quiet -p tiv-cli --bin tiv -- \
    replay reference-app-case \
    --plan spike/planned-case-http-v1.json \
    --journal /tmp/tiv-reference-app-case.jsonl \
    --postgres-port 15432 \
    --reference-app-url http://127.0.0.1:18080 \
    --fixture-control-url http://127.0.0.1:12112
```

Each self-contained public run restarts only the previously attested reference
application container, waits for both HTTP and Docker health, re-attests the
same container identity, and then allocates the fixture's next authenticated
control sequence. This makes consecutive evidence and planned-case commands
safe on one isolated stack without weakening the monotonic sequence contract.

Run the compiled `client_request_forwarded` SIGKILL/restart case with another
new journal path:

```sh
TIV_FIXTURE_CONTROL_TOKEN=run-scoped-control-token \
TIV_POSTGRES_ADMIN_PASSWORD=tiv-local-only-password \
TIV_POSTGRES_APPLICATION_PASSWORD=tiv-app-local-only-password \
  cargo run --quiet -p tiv-cli --bin tiv -- \
    replay reference-app-case \
    --plan crates/tiv-core/tests/golden/planned-case-v3.json \
    --journal /tmp/tiv-reference-process-case.jsonl \
    --postgres-port 15432 \
    --reference-app-url http://127.0.0.1:18080 \
    --fixture-control-url http://127.0.0.1:12112
```

The runner implements `client_request_forwarded`, supported application
`client_response_observed`, fixture `webhook_response_observed`, and a
repository-owned `sql_probe` loaded from the typed project configuration.
Plans containing that cut point must pass `--config <path>` and provide the
configuration's referenced environment variables. The probe must begin false,
then become true in a committed read-only snapshot before the runner SIGKILLs the exact attested
application container, proves it stopped, starts the same container, waits for
HTTP health and full stack re-attestation, and then continues the durable
journal. Webhook-request and abstract process-cut placements fail before stack
or database mutation.

The command provisions a generated case database, installs a sequenced
`commit_then_close`/`normal` fault plan, drives the application checkout, signs
and delivers the fixture's exact raw webhook bytes, observes two provider and
local objects for the generated case-derived operation, and runs the five-query snapshot. It repeats that
exact trace three times, cloning the sealed baseline before attempts two and
three, then classifies the expected invariant/checkpoint identity as `stable`
(3/3), `reproducible` (2/3), or `inconclusive` (0/3 or 1/3). Standard output is
one schema-v2 allowlisted JSON evidence document containing the three provider
counts, two database-reset transitions, failure identity, match count, and
classification; PostgreSQL passwords, the fixture control token, raw webhook
material, and generated database names are omitted.
Only completed oracle attempts contribute to that classification. An
attestation, transport, PostgreSQL, reset, or oracle-execution error aborts the
command without emitting evidence instead of being relabeled `inconclusive`.
Before sending either database password, the command bypasses ambient remote
Docker contexts and uses the local `/var/run/docker.sock` control plane to
attest the expected running Compose project and all three healthy service
containers, including each image, command, purpose label, and exact loopback
port binding. Every Docker inspection has bounded output, a ten-second command
deadline, and explicit kill/reap cleanup on timeout.

Public commands read the authenticated fixture state and allocate the next
strictly monotonic control sequence, so they can run serially on one attested
stack. Each journal path must be new.

Remove only this exact disposable project when finished:

```sh
docker compose \
  --project-name tiv-reference-app-spike \
  --project-directory "$PWD" \
  --file spike/reference-app.compose.yaml \
  down --volumes --remove-orphans
```

This remains bounded truth-spike evidence. It proves one synthetic application,
the five-query configured oracle, and faulty/corrected execution pairs for
reference fault rows 1, 3, and 6. It does not yet prove arbitrary customer
repositories, the other three reference fault variants, exhaustive schedule
coverage, production secret management, or distributed image provenance.
