#![deny(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use serde_json::json;

use gcp_fetch_secret::output::{WriteMode, WriteOptions};
use gcp_fetch_secret::{config, log, source, sync::Syncer};

/// Fetch secrets from GCP Secret Manager into env files and secret files,
/// then keep them up to date as new secret versions are added.
#[derive(Debug, Parser)]
#[command(name = "gcp-fetch-secret", version)]
struct Args {
    /// YAML file listing the secrets to fetch.
    input_file: PathBuf,

    /// Where to write secrets that have no `file_name`.
    output_file: PathBuf,

    /// Directory for secrets that have a `file_name`.
    output_dir: PathBuf,

    /// Seconds between checks for new secret versions.
    #[arg(short, long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    sleep: u64,

    /// Write the files once and exit instead of watching for changes.
    #[arg(short, long)]
    oneshot: bool,

    /// Permission bits (octal) for written files.
    #[arg(long, default_value = "444", value_parser = parse_mode)]
    file_mode: u32,

    /// How existing files are updated. `in-place` rewrites the same file so
    /// single-file bind mounts see changes; `replace` renames a new file over
    /// it so readers never see a partial write.
    #[arg(long, value_enum, default_value_t = WriteMode::InPlace)]
    write_mode: WriteMode,

    /// Override the Secret Manager endpoint, e.g. a regional endpoint like
    /// https://secretmanager.us-east1.rep.googleapis.com. An http:// endpoint
    /// (local emulator) is used without credentials.
    #[arg(long, env = "GCP_FETCH_SECRET_ENDPOINT")]
    endpoint: Option<String>,
}

fn parse_mode(s: &str) -> Result<u32, String> {
    let digits = s.strip_prefix("0o").unwrap_or(s);
    match u32::from_str_radix(digits, 8) {
        Ok(m) if m <= 0o777 => Ok(m),
        _ => Err(format!("{s:?} is not an octal file mode like 444 or 640")),
    }
}

/// Resolves on SIGTERM or SIGINT.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
    }
}

async fn run(args: Args) -> Result<(), String> {
    let plan = config::load(&args.input_file, &args.output_file, &args.output_dir)
        .map_err(|e| e.to_string())?;
    let client = source::secret_manager(args.endpoint.as_deref())
        .await
        .map_err(|e| format!("failed to create Secret Manager client: {e}"))?;
    let write_opts = WriteOptions {
        file_mode: args.file_mode,
        write_mode: args.write_mode,
    };
    Syncer::new(client, plan, write_opts)
        .run(Duration::from_secs(args.sleep), args.oneshot)
        .await
        .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    tokio::select! {
        result = run(args) => match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log::error(&e, json!({}));
                ExitCode::from(2)
            }
        },
        _ = shutdown_signal() => {
            log::info("Shutdown signal received", json!({}));
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_is_valid() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn positional_args_and_flags_match_python_version() {
        let a = Args::try_parse_from([
            "gcp-fetch-secret",
            "in.yaml",
            "out.env",
            "out.d",
            "-s",
            "5",
            "-o",
        ])
        .unwrap();
        assert_eq!(a.input_file, PathBuf::from("in.yaml"));
        assert_eq!(a.output_file, PathBuf::from("out.env"));
        assert_eq!(a.output_dir, PathBuf::from("out.d"));
        assert_eq!(a.sleep, 5);
        assert!(a.oneshot);
        assert_eq!(a.file_mode, 0o444);
        assert_eq!(a.write_mode, WriteMode::InPlace);
        let a = Args::try_parse_from(["s", "a", "b", "c", "--write-mode", "replace"]).unwrap();
        assert_eq!(a.write_mode, WriteMode::Replace);
    }

    #[test]
    fn rejects_zero_sleep() {
        assert!(Args::try_parse_from(["s", "a", "b", "c", "--sleep", "0"]).is_err());
    }

    #[test]
    fn parses_modes() {
        assert_eq!(parse_mode("444"), Ok(0o444));
        assert_eq!(parse_mode("0640"), Ok(0o640));
        assert_eq!(parse_mode("0o600"), Ok(0o600));
        assert!(parse_mode("999").is_err());
        assert!(parse_mode("1777").is_err());
        assert!(parse_mode("rw").is_err());
    }
}
