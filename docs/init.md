# `tiv init`

Run `tiv init` from the root of a Git repository to create the editable
version-one TxProof skeleton. The command does not inspect or infer application
behavior, start Docker, connect to PostgreSQL, or write credentials.

It creates exactly these nine files:

```text
tiv.toml
checkout.json
quiescence.sql
kill_probe.sql
invariants/01_provider_object_unique.sql
invariants/02_webhook_effect_at_most_once.sql
invariants/03_paid_order_amount_conservation.sql
invariants/04_terminal_success_monotonic.sql
invariants/05_balanced_ledger.sql
```

The config contains only environment-variable names for secret-bearing values.
The request and SQL files are explicit TODO templates. Every SQL template calls
the deliberately nonexistent `tiv_configuration_required` function, so an
unmapped template errors instead of silently passing an invariant.

## No-overwrite contract

There is intentionally no `--force` flag. Before writing anything, `init`
checks every file and the `invariants` directory. If any destination exists, it
exits with code `2` and leaves the repository unchanged. Files also use
create-new semantics to prevent a race from overwriting work created after the
preflight. If an I/O error follows a successful create, TxProof removes only
the files created by that attempt and reports a distinct rollback failure if
cleanup cannot finish.

## Next actions

1. Add `.tiv/` to the repository's `.gitignore`.
2. Replace every TODO mapping in `tiv.toml`, `checkout.json`, and the SQL files
   with repository-owned behavior.
3. Export the three local-test values named by `admin_url_env`, `case_url_env`,
   and `webhook_secret_env`.
4. Run `tiv doctor --config tiv.toml`.

A successful `init` prints one JSON report listing the created paths and that
next command. It is scaffold evidence only; readiness is established by
`doctor`, and neither command authorizes database mutation.
