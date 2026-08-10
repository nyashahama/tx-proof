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
  cargo test -p tiv-runtime postgres::spike::tests:: -- --ignored --test-threads=1
```

The operation-level `provider-object-unique` invariant is a truth-spike
interpretation, not yet a final customer invariant template. It reports an
operation that has more than one provider PaymentIntent ID. The other four
queries are explicit no-ops in this slice so the exact five-query transaction
shape is exercised without claiming unimplemented product semantics.

The PostgreSQL-only stack does not mark its sole bridge `internal: true`.
Docker 29 accepted the declared port binding but did not install a runtime
publication for a container attached only to a gateway-less internal network.
The later multi-service topology can use a separate internal data network while
retaining a loopback-capable control network.
