use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;
use tiv_core::trace::ActionId;
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt as _,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const OBSERVATION_SCHEMA_VERSION: u16 = 1;
const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const MAX_RECORD_BYTES: usize = 8 * 1024;
const MAX_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalLimits {
    channel_capacity: usize,
    max_records: usize,
}

impl JournalLimits {
    /// Defines the bounded queue and final record count.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidJournalLimits`] when either bound is zero.
    pub const fn new(
        channel_capacity: usize,
        max_records: usize,
    ) -> Result<Self, InvalidJournalLimits> {
        if channel_capacity == 0 || max_records == 0 {
            return Err(InvalidJournalLimits);
        }
        Ok(Self {
            channel_capacity,
            max_records,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidJournalLimits;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalContext {
    run: String,
    case: String,
    action: ActionId,
}

impl JournalContext {
    /// Creates the run, case, and action identity attached to one observation.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidJournalId`] unless both string IDs are narrow,
    /// non-empty ASCII identifiers.
    pub fn new(
        run_id: impl Into<String>,
        case_id: impl Into<String>,
        action_id: ActionId,
    ) -> Result<Self, InvalidJournalId> {
        let run_id = run_id.into();
        let case_id = case_id.into();
        if !valid_id(&run_id) || !valid_id(&case_id) {
            return Err(InvalidJournalId);
        }
        Ok(Self {
            run: run_id,
            case: case_id,
            action: action_id,
        })
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidJournalId;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationProducer {
    Orchestrator,
    Driver,
    Fixture,
    Postgres,
    Compose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationEvent {
    ActionIntent,
    ActionOutcome,
    ClientResponseObserved,
    ProviderResponseHeld { gate_id: u64 },
    ProviderResponseReleased { gate_id: u64 },
    WebhookRequestForwarded { gate_id: u64 },
    WebhookRequestDiscarded { gate_id: u64 },
    WebhookResponseObserved { gate_id: u64 },
    WebhookResponseDiscarded { gate_id: u64 },
    SqlProbeTrue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    context: JournalContext,
    producer: ObservationProducer,
    producer_sequence: u64,
    monotonic_elapsed_micros: u64,
    event: ObservationEvent,
}

impl Observation {
    #[must_use]
    pub const fn new(
        context: JournalContext,
        producer: ObservationProducer,
        producer_sequence: u64,
        monotonic_elapsed_micros: u64,
        event: ObservationEvent,
    ) -> Self {
        Self {
            context,
            producer,
            producer_sequence,
            monotonic_elapsed_micros,
            event,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ObservationKind {
    ActionIntent,
    ActionOutcome,
    ClientResponseObserved,
    ProviderResponseHeld,
    ProviderResponseReleased,
    WebhookRequestForwarded,
    WebhookRequestDiscarded,
    WebhookResponseObserved,
    WebhookResponseDiscarded,
    SqlProbeTrue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ObservationPayload {
    None,
    Gate { gate_id: u64 },
}

impl ObservationEvent {
    const fn parts(self) -> (ObservationKind, ObservationPayload) {
        match self {
            Self::ActionIntent => (ObservationKind::ActionIntent, ObservationPayload::None),
            Self::ActionOutcome => (ObservationKind::ActionOutcome, ObservationPayload::None),
            Self::ClientResponseObserved => (
                ObservationKind::ClientResponseObserved,
                ObservationPayload::None,
            ),
            Self::ProviderResponseHeld { gate_id } => (
                ObservationKind::ProviderResponseHeld,
                ObservationPayload::Gate { gate_id },
            ),
            Self::ProviderResponseReleased { gate_id } => (
                ObservationKind::ProviderResponseReleased,
                ObservationPayload::Gate { gate_id },
            ),
            Self::WebhookRequestForwarded { gate_id } => (
                ObservationKind::WebhookRequestForwarded,
                ObservationPayload::Gate { gate_id },
            ),
            Self::WebhookRequestDiscarded { gate_id } => (
                ObservationKind::WebhookRequestDiscarded,
                ObservationPayload::Gate { gate_id },
            ),
            Self::WebhookResponseObserved { gate_id } => (
                ObservationKind::WebhookResponseObserved,
                ObservationPayload::Gate { gate_id },
            ),
            Self::WebhookResponseDiscarded { gate_id } => (
                ObservationKind::WebhookResponseDiscarded,
                ObservationPayload::Gate { gate_id },
            ),
            Self::SqlProbeTrue => (ObservationKind::SqlProbeTrue, ObservationPayload::None),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ObservationRecord {
    schema_version: u16,
    observed_sequence: u64,
    run_id: String,
    case_id: String,
    action_id: ActionId,
    producer: ObservationProducer,
    producer_sequence: u64,
    observation_kind: ObservationKind,
    monotonic_elapsed_micros: u64,
    payload: ObservationPayload,
    previous_hash: String,
    record_hash: String,
}

impl ObservationRecord {
    #[must_use]
    pub const fn observed_sequence(&self) -> u64 {
        self.observed_sequence
    }

    #[must_use]
    pub fn previous_hash(&self) -> &str {
        &self.previous_hash
    }

    #[must_use]
    pub fn record_hash(&self) -> &str {
        &self.record_hash
    }
}

#[derive(Serialize)]
struct RecordHashInput<'a> {
    schema_version: u16,
    observed_sequence: u64,
    run_id: &'a str,
    case_id: &'a str,
    action_id: ActionId,
    producer: ObservationProducer,
    producer_sequence: u64,
    observation_kind: ObservationKind,
    monotonic_elapsed_micros: u64,
    payload: ObservationPayload,
    previous_hash: &'a str,
}

pub struct ObservationJournal {
    sender: Option<mpsc::Sender<AppendRequest>>,
    writer: JoinHandle<Result<JournalSummary, String>>,
}

impl ObservationJournal {
    /// Creates a new private append-only journal and starts its single writer.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::Create`] when the target already exists or
    /// cannot be created.
    pub async fn create(
        path: impl AsRef<Path>,
        limits: JournalLimits,
    ) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .await
            .map_err(|source| JournalError::Create {
                path: path.clone(),
                source,
            })?;
        let (sender, receiver) = mpsc::channel(limits.channel_capacity);
        let writer = tokio::spawn(run_writer(file, receiver, limits.max_records));
        Ok(Self {
            sender: Some(sender),
            writer,
        })
    }

    /// Appends one observation and returns only after its record is flushed
    /// and synchronized to the journal file.
    ///
    /// # Errors
    ///
    /// Returns a typed journal error when the writer has stopped or a bound or
    /// I/O operation fails.
    pub async fn append(
        &self,
        observation: Observation,
    ) -> Result<ObservationRecord, JournalError> {
        let sender = self.sender.as_ref().ok_or(JournalError::WriterStopped)?;
        let (reply, response) = oneshot::channel();
        sender
            .send(AppendRequest { observation, reply })
            .await
            .map_err(|_| JournalError::WriterStopped)?;
        response
            .await
            .map_err(|_| JournalError::WriterStopped)?
            .map_err(AppendFailure::into_public)
    }

    /// Drains the queue, synchronizes the file, and joins the writer task.
    ///
    /// # Errors
    ///
    /// Returns a typed journal error if the writer task or final sync fails.
    pub async fn finish(mut self) -> Result<JournalSummary, JournalError> {
        self.sender.take();
        self.writer
            .await
            .map_err(JournalError::WriterJoin)?
            .map_err(JournalError::Write)
    }
}

struct AppendRequest {
    observation: Observation,
    reply: oneshot::Sender<Result<ObservationRecord, AppendFailure>>,
}

async fn run_writer(
    mut file: File,
    mut receiver: mpsc::Receiver<AppendRequest>,
    max_records: usize,
) -> Result<JournalSummary, String> {
    let mut record_count = 0_usize;
    let mut previous_hash = GENESIS_HASH.to_owned();
    let mut producer_sequences = BTreeMap::new();
    while let Some(request) = receiver.recv().await {
        if record_count == max_records {
            let _ignored = request.reply.send(Err(AppendFailure::RecordLimit));
            continue;
        }
        let expected_producer_sequence = producer_sequences
            .get(&request.observation.producer)
            .copied()
            .unwrap_or(0_u64)
            .checked_add(1)
            .ok_or_else(|| "producer observation sequence exhausted".to_owned())?;
        if request.observation.producer_sequence != expected_producer_sequence {
            let _ignored = request
                .reply
                .send(Err(AppendFailure::UnexpectedProducerSequence {
                    expected: expected_producer_sequence,
                    received: request.observation.producer_sequence,
                }));
            continue;
        }
        let observed_sequence = u64::try_from(record_count)
            .map_err(|error| error.to_string())?
            .checked_add(1)
            .ok_or_else(|| "observation sequence exhausted".to_owned())?;
        let record = match build_record(request.observation, observed_sequence, &previous_hash) {
            Ok(record) => record,
            Err(error) => {
                let _ignored = request.reply.send(Err(error));
                continue;
            }
        };
        let mut encoded = match serde_json::to_vec(&record) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ignored = request
                    .reply
                    .send(Err(AppendFailure::Encode(error.to_string())));
                continue;
            }
        };
        encoded.push(b'\n');
        if encoded.len() > MAX_RECORD_BYTES {
            let _ignored = request.reply.send(Err(AppendFailure::RecordTooLarge));
            continue;
        }
        if let Err(error) = write_durable(&mut file, &encoded).await {
            let message = error.to_string();
            let _ignored = request
                .reply
                .send(Err(AppendFailure::Write(message.clone())));
            return Err(message);
        }
        record_count += 1;
        producer_sequences.insert(record.producer, record.producer_sequence);
        previous_hash.clone_from(&record.record_hash);
        let _ignored = request.reply.send(Ok(record));
    }
    file.sync_all().await.map_err(|error| error.to_string())?;
    Ok(JournalSummary {
        record_count,
        last_record_hash: (record_count > 0).then_some(previous_hash),
    })
}

async fn write_durable(file: &mut File, bytes: &[u8]) -> Result<(), std::io::Error> {
    file.write_all(bytes).await?;
    file.flush().await?;
    file.sync_data().await
}

fn build_record(
    observation: Observation,
    observed_sequence: u64,
    previous_hash: &str,
) -> Result<ObservationRecord, AppendFailure> {
    let (observation_kind, payload) = observation.event.parts();
    let hash_input = RecordHashInput {
        schema_version: OBSERVATION_SCHEMA_VERSION,
        observed_sequence,
        run_id: &observation.context.run,
        case_id: &observation.context.case,
        action_id: observation.context.action,
        producer: observation.producer,
        producer_sequence: observation.producer_sequence,
        observation_kind,
        monotonic_elapsed_micros: observation.monotonic_elapsed_micros,
        payload,
        previous_hash,
    };
    let canonical = serde_json::to_vec(&hash_input)
        .map_err(|error| AppendFailure::Encode(error.to_string()))?;
    let record_hash = blake3::hash(&canonical).to_hex().to_string();
    Ok(ObservationRecord {
        schema_version: OBSERVATION_SCHEMA_VERSION,
        observed_sequence,
        run_id: observation.context.run,
        case_id: observation.context.case,
        action_id: observation.context.action,
        producer: observation.producer,
        producer_sequence: observation.producer_sequence,
        observation_kind,
        monotonic_elapsed_micros: observation.monotonic_elapsed_micros,
        payload,
        previous_hash: previous_hash.to_owned(),
        record_hash,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AppendFailure {
    Encode(String),
    RecordLimit,
    RecordTooLarge,
    UnexpectedProducerSequence { expected: u64, received: u64 },
    Write(String),
}

impl AppendFailure {
    fn into_public(self) -> JournalError {
        match self {
            Self::Encode(message) => JournalError::Encode(message),
            Self::RecordLimit => JournalError::RecordLimit,
            Self::RecordTooLarge => JournalError::RecordTooLarge,
            Self::UnexpectedProducerSequence { expected, received } => {
                JournalError::UnexpectedProducerSequence { expected, received }
            }
            Self::Write(message) => JournalError::Write(message),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSummary {
    record_count: usize,
    last_record_hash: Option<String>,
}

impl JournalSummary {
    #[must_use]
    pub const fn record_count(&self) -> usize {
        self.record_count
    }

    #[must_use]
    pub fn last_record_hash(&self) -> Option<&str> {
        self.last_record_hash.as_deref()
    }
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("could not create observation journal {path}: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not encode observation record: {0}")]
    Encode(String),
    #[error("observation record limit reached")]
    RecordLimit,
    #[error("observation record exceeds the fixed byte limit")]
    RecordTooLarge,
    #[error("unexpected producer sequence: expected {expected}, received {received}")]
    UnexpectedProducerSequence { expected: u64, received: u64 },
    #[error("observation journal write failed: {0}")]
    Write(String),
    #[error("observation journal writer stopped")]
    WriterStopped,
    #[error("observation journal writer task failed: {0}")]
    WriterJoin(tokio::task::JoinError),
}
