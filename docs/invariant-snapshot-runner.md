# Invariant snapshot runner

The Phase 1 runtime now has one generic path for evaluating the fixed five v1
invariants. It consumes the same resolved config that `tiv doctor` approves,
reads the five repository-owned SQL files in canonical order, loads the bounded
provider projection, and evaluates all five queries inside one PostgreSQL
`READ ONLY REPEATABLE READ` transaction.

This is an internal runtime slice, not yet a new public CLI command. The
executor, artifact writer, and least-privilege invariant-role setup remain
separate follow-on work.

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

The runner does not yet provision or authenticate as the planned
least-privilege invariant role. Until that role contract is wired into the
executor, repository-owned SQL is trusted input and this slice must not be
described as the complete production oracle.
