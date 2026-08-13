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

The command provisions a generated case database, installs a sequenced
`commit_then_close`/`normal` fault plan, drives the application checkout, signs
and delivers the fixture's exact raw webhook bytes, observes two provider and
local objects for `op_1`, and runs the five-query snapshot. It repeats that
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

The reference stack must be fresh for each evidence command because fixture
control commands are strictly sequenced. Reset it with the scoped `down` command
below before starting another run.

Remove only this exact disposable project when finished:

```sh
docker compose \
  --project-name tiv-reference-app-spike \
  --project-directory "$PWD" \
  --file spike/reference-app.compose.yaml \
  down --volumes --remove-orphans
```

This remains bounded truth-spike evidence. It proves one synthetic known-bug
application and one implemented operation-level invariant. It does not yet
prove arbitrary customer repositories, all five business invariants, schedule
generation or shrinking, production secret management, or distributed image
provenance.
