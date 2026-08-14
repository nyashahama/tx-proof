# Invariant snapshot runner

The Phase 1 runtime now has one generic path for evaluating the fixed five v1
invariants. It consumes the same resolved config that `tiv doctor` approves,
reads the five repository-owned SQL files in canonical order, loads the bounded
provider projection, and evaluates all five queries inside one PostgreSQL
`READ ONLY REPEATABLE READ` transaction.

This is an internal runtime slice, not yet a new public CLI command. The
executor and artifact writer remain separate follow-on work.

## Query contract

Each invariant is trusted repository code, but it must still satisfy a narrow
fail-closed contract:

- exactly one parameter-free `SELECT` or `WITH ... SELECT` statement;
- exactly the five v1 IDs, once each and in canonical order;
- no DML, DDL, session control, sequence mutation, backend control, large-object
  mutation, notification, or advisory-lock operation;
- at least one and at most 32 uniquely named diagnostic columns;
- only supported non-floating scalar or array diagnostic types;
- zero rows means held; one or more rows means violated.

The repository lexer is only the first safety layer. PostgreSQL preparation is
the final parser for the single-statement and parameter-free constraints, and
the database transaction is read-only and always explicitly rolled back.

## Least-privilege role boundary

The configured invariant role is constrained to a safe unquoted identifier.
Before any temporary projection is created, the runtime attests that the role:

- is `NOLOGIN`, `NOSUPERUSER`, `NOCREATEDB`, `NOCREATEROLE`, `NOINHERIT`,
  `NOREPLICATION`, and `NOBYPASSRLS`;
- has no role memberships in either direction and owns no database, schema,
  relation, function, or type in the current database;
- has no effective database `CREATE` or `TEMP`, schema `CREATE`, persistent
  table write, or sequence mutation capability;
- is being inspected from the original authorization state of one exact
  backend and database.

The single-use permit is bound to that backend and database, and the full role
attestation runs again immediately before projection loading. The admin session
loads the bounded provider rows, grants that role `SELECT` on only the
connection-local projection, begins the read-only transaction, and uses
`SET LOCAL ROLE` before preparing any repository query. The runner then proves
both the effective `current_user` and `transaction_read_only` state.

PostgreSQL exposes a built-in `PUBLIC` update surface on
`pg_catalog.pg_settings`; that exact system view is the only table-write
attestation exception. Repository SQL still cannot contain `UPDATE`, `SET`, or
`set_config`, and the read-only transaction supplies the database-side guard.

This follows PostgreSQL's documented distinction between the session user and
effective role, its object privilege model, and its warning that read-only
transactions can still write temporary tables:

- <https://www.postgresql.org/docs/current/sql-set-role.html>
- <https://www.postgresql.org/docs/current/ddl-priv.html>
- <https://www.postgresql.org/docs/current/sql-set-transaction.html>

## Fixed bounds

The v1 implementation rejects expansion beyond these constants:

| Boundary | Maximum |
| --- | ---: |
| Query file | 64 KiB |
| Statement timeout | 2 seconds |
| Lock timeout | 500 milliseconds |
| Diagnostic columns | 32 |
| Witness rows per invariant | 100 |
| Encoded bytes per top-level value | 16 KiB |
| Encoded witness bytes across the snapshot | 256 KiB |

Witnesses remain typed, in-memory values that intentionally implement neither
`Debug` nor `Serialize`. A later artifact boundary must redact them before any
persistence or standard output.

## Current proof and remaining boundary

The isolated PostgreSQL gate proves all five no-op invariants run in one real
snapshot. It also proves rejection of query parameters, floating diagnostics,
101-row witnesses, oversized values, oversized aggregate evidence, missing
relations, and statements that exceed the timeout. A successful follow-up run
on the same connection proves failed transactions are rolled back and the
session is recovered.

The isolated reference topology now provisions the fixed `tiv_invariant`
`NOLOGIN` role, grants only schema `USAGE` plus `SELECT` on application state,
and proves those grants survive template reset. A customer adapter must still
install equivalent schema-specific grants before the public executor exists.
Repository-owned SQL remains trusted input: role attestation cannot inspect the
body or external side effects of customer-defined functions, and `NOBYPASSRLS`
means repository policies still determine which rows are visible. This slice
must therefore not be described as the complete production oracle.
