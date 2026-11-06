#![deny(unsafe_code)]
//! Fetch secrets from GCP Secret Manager into env files / secret files and
//! keep them up to date.

pub mod config;
pub mod log;
pub mod output;
pub mod source;
pub mod sync;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read config {}: {source}", path.display())]
    ConfigRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {}:\n{message}", path.display())]
    ConfigInvalid { path: PathBuf, message: String },
    #[error("failed to fetch secrets: {}", .0.join("; "))]
    Fetch(Vec<String>),
    #[error("secret {secret_id} cannot be written as {env_name}: {reason}")]
    InvalidEnvValue {
        secret_id: String,
        env_name: String,
        reason: String,
    },
    #[error("failed to write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}
