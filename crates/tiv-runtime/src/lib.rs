//! Effectful runtime adapters for `TxProof`.

pub mod artifacts;
pub mod baseline;
pub mod campaign;
pub mod case_http;
pub mod compatibility;
pub mod config;
pub mod configured_campaign;
pub(crate) mod configured_database;
pub mod configured_minimized_replay;
pub(crate) mod configured_process;
pub mod configured_replay;
pub mod configured_shrink;
pub mod doctor;
pub mod evidence;
pub mod init;
pub mod journal;
pub mod postgres;
pub mod provider_http;
pub mod reference_app;
pub mod reference_case;
pub mod replay;
pub(crate) mod reports;
pub(crate) mod repository;
pub(crate) mod run_supervisor;
pub mod webhook_http;
