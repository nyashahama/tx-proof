-- TxProof destructive-reset marker for a disposable local test database.
--
-- TODO: replace both placeholder values, add this file to the repository's
-- test-only migrations, and remove the fail-closed SELECT below. The Compose
-- project must match the isolated stack and the application role must match
-- the role used by DATABASE_URL. Never apply this migration to shared or
-- production data.
SELECT tiv_configuration_required('safety-marker');

CREATE TABLE tiv_verifier_marker (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    marker_uuid uuid NOT NULL,
    marker_kind text NOT NULL CHECK (marker_kind IN ('baseline', 'case')),
    compose_project text NOT NULL,
    application_role text NOT NULL
);

INSERT INTO tiv_verifier_marker (
    marker_uuid,
    marker_kind,
    compose_project,
    application_role
) VALUES (
    gen_random_uuid(),
    'case',
    'TODO-compose-project',
    'TODO-application-role'
);

REVOKE ALL ON TABLE tiv_verifier_marker FROM PUBLIC;
