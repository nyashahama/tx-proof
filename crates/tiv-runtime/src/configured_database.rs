//! Independent configured `PostgreSQL` sessions for one attested case.

use std::time::Duration;

use thiserror::Error;

use crate::{
    baseline::{BaselineError, ConfiguredCaseDatabase, PostgresSession},
    postgres::{
        oracle::{ProviderPaymentIntent, QuiescencePermit},
        probe::{ConfiguredSqlProbe, SqlProbe},
        quiescence::ConfiguredQuiescence,
        snapshot::{
            ConfiguredSnapshot, InvariantRoleError, SnapshotError, SnapshotReport,
            attest_invariant_role, run_snapshot,
        },
    },
    reference_case::{
        CaseQuiescenceFuture, CaseQuiescenceGate, CaseSqlProbe, CaseSqlProbeError,
        CaseSqlProbeFuture,
    },
};

pub(crate) struct ConfiguredSqlProbeSession {
    session: PostgresSession,
    probe: SqlProbe,
    timeout: Duration,
    poll_interval: Duration,
}

impl CaseSqlProbe for ConfiguredSqlProbeSession {
    fn require_false(&mut self) -> CaseSqlProbeFuture<'_> {
        Box::pin(async move {
            self.probe
                .require_false(&mut self.session.client)
                .await
                .map_err(|_| CaseSqlProbeError)
        })
    }

    fn observe_true(&mut self) -> CaseSqlProbeFuture<'_> {
        Box::pin(async move {
            self.probe
                .observe_true(&mut self.session.client, self.timeout, self.poll_interval)
                .await
                .map_err(|_| CaseSqlProbeError)
        })
    }

    fn observed(&self) -> bool {
        self.probe.observed()
    }
}

impl ConfiguredSqlProbeSession {
    pub(crate) async fn close(self) -> Result<(), BaselineError> {
        self.session.close().await
    }
}

pub(crate) struct ConfiguredQuiescenceSession {
    session: PostgresSession,
    configured: ConfiguredQuiescence,
    poll_interval: Duration,
}

impl CaseQuiescenceGate for ConfiguredQuiescenceSession {
    fn await_stable(&mut self) -> CaseQuiescenceFuture<'_> {
        Box::pin(async move {
            self.configured
                .await_stable(&mut self.session.client, self.poll_interval)
                .await
        })
    }
}

impl ConfiguredQuiescenceSession {
    pub(crate) async fn close(self) -> Result<(), BaselineError> {
        self.session.close().await
    }
}

pub(crate) struct ConfiguredSnapshotSession {
    session: PostgresSession,
    configured: ConfiguredSnapshot,
}

impl ConfiguredSnapshotSession {
    pub(crate) async fn run(
        mut self,
        provider_objects: &[ProviderPaymentIntent],
        quiescence: QuiescencePermit,
    ) -> Result<SnapshotReport, ConfiguredDatabaseError> {
        let role_permit =
            attest_invariant_role(&self.session.client, self.configured.role().clone())
                .await
                .map_err(ConfiguredDatabaseError::InvariantRole)?;
        let evaluation = run_snapshot(
            &mut self.session.client,
            provider_objects,
            self.configured.suite(),
            self.configured.budgets(),
            role_permit,
            quiescence,
        )
        .await;
        let close = self.session.close().await;
        match (evaluation, close) {
            (Ok(report), Ok(())) => Ok(report),
            (Err(error), _) => Err(ConfiguredDatabaseError::Snapshot(error)),
            (Ok(_), Err(error)) => Err(ConfiguredDatabaseError::Connection(error)),
        }
    }
}

impl ConfiguredCaseDatabase {
    pub(crate) async fn open_sql_probe(
        &self,
        configured: ConfiguredSqlProbe,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<ConfiguredSqlProbeSession, BaselineError> {
        Ok(ConfiguredSqlProbeSession {
            session: self.connect().await?,
            probe: configured.into_probe(),
            timeout,
            poll_interval,
        })
    }

    pub(crate) async fn open_quiescence(
        &self,
        configured: ConfiguredQuiescence,
        poll_interval: Duration,
    ) -> Result<ConfiguredQuiescenceSession, BaselineError> {
        Ok(ConfiguredQuiescenceSession {
            session: self.connect().await?,
            configured,
            poll_interval,
        })
    }

    pub(crate) async fn open_snapshot(
        &self,
        configured: ConfiguredSnapshot,
    ) -> Result<ConfiguredSnapshotSession, BaselineError> {
        Ok(ConfiguredSnapshotSession {
            session: self.connect().await?,
            configured,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfiguredDatabaseError {
    #[error("configured invariant-role attestation failed: {0}")]
    InvariantRole(#[source] InvariantRoleError),
    #[error("configured snapshot failed: {0}")]
    Snapshot(#[source] SnapshotError),
    #[error("configured database connection did not close cleanly: {0}")]
    Connection(#[source] BaselineError),
}

#[cfg(test)]
mod tests {
    use crate::baseline::ConfiguredCaseDatabase;

    #[test]
    fn configured_case_exposes_independent_probe_quiescence_and_snapshot_sessions() {
        let _ = ConfiguredCaseDatabase::open_sql_probe;
        let _ = ConfiguredCaseDatabase::open_quiescence;
        let _ = ConfiguredCaseDatabase::open_snapshot;
    }
}
