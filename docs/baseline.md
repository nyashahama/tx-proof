# `tiv baseline`

`tiv baseline` seals the already migrated and synthetically seeded disposable
case database, then proves that a destructive reset restores that exact state.
It is intentionally a two-stage command so operator consent can be bound to a
fresh database identity instead of a filename.

## Preconditions

- `tiv.toml` passed `tiv doctor` and names the actual running Compose project.
- The PostgreSQL service is healthy and publishes container port `5432` only
  on the configured loopback port.
- Migrations, synthetic seed data, and `tiv-safety-marker.sql` have been applied
  to the configured case database.
- The marker contains the same Compose project and application role as config.
- The application role is a non-superuser, non-owner, membership-free login
  that cannot modify the marker table.
- The configured baseline database does not already exist.

## Challenge stage

Run the command without acknowledgement:

```text
tiv baseline --config tiv.toml
```

TxProof first pins Docker to `/var/run/docker.sock`, attests the exact running
Compose PostgreSQL container and loopback port, then observes the case database
server fingerprint, endpoint, OID, owner, marker UUID, Compose project, and
application role. It emits `status: "acknowledgement_required"` and an exact
`reset_acknowledgement` phrase. This stage does not stop services or mutate
PostgreSQL.

## Seal and reset-proof stage

Repeat the complete phrase as one quoted argument:

```text
tiv baseline --config tiv.toml \
  --acknowledge-reset 'RESET ...the complete emitted identity...'
```

An abbreviated or stale phrase exits before mutation. For an exact phrase,
TxProof stops the configured application and workers, re-attests the same
PostgreSQL container, freshly rechecks every database identity field, and
consumes the acknowledgement once. It then:

1. clones the migrated and seeded case database into the configured baseline;
2. gives the baseline a distinct marker, catalog attestation, and sealed
   template state;
3. creates a private `tiv_reset_probe` relation only in the case database;
4. drops and recreates the case from the sealed baseline;
5. gives the recreated case a fresh marker and verifies a new database OID;
6. proves the private reset probe disappeared; and
7. restarts the application and workers, waits for health, and re-attests the
   same PostgreSQL container.

Successful output is one secret-free JSON document with
`status: "baseline_ready"`, the before/after case identities, sealed baseline
identity, and reset proof. Existing baselines fail closed; replacing or cleaning
one is a separate lifecycle operation.
