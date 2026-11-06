//! Fetching secrets, writing files, and watching for new versions.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::json;

use crate::config::Plan;
use crate::output::WriteOptions;
use crate::source::SecretSource;
use crate::{Error, log, output};

#[derive(Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Unchanged,
    Updated,
}

pub struct Syncer<S> {
    source: S,
    plan: Plan,
    write_opts: WriteOptions,
    /// secret id -> version last written to disk. Only updated after every
    /// file was written successfully, so a failed sync is retried.
    versions: HashMap<String, String>,
}

impl<S: SecretSource> Syncer<S> {
    pub fn new(source: S, plan: Plan, write_opts: WriteOptions) -> Self {
        Self {
            source,
            plan,
            write_opts,
            versions: HashMap::new(),
        }
    }

    /// Versions currently written to disk.
    pub fn versions(&self) -> &HashMap<String, String> {
        &self.versions
    }

    /// Fetch every secret and (re)write every output file.
    ///
    /// Nothing is written unless every secret was fetched and every file
    /// rendered, so a partial failure never leaves a half-updated env file.
    pub async fn sync(&mut self) -> Result<(), Error> {
        let ids = self.plan.secret_ids();
        let results = join_all(ids.iter().map(|id| self.source.access(id))).await;

        let mut secrets = HashMap::with_capacity(ids.len());
        let mut failures = Vec::new();
        for (id, result) in ids.into_iter().zip(results) {
            match result {
                Ok(secret) => {
                    secrets.insert(id, secret);
                }
                Err(e) => failures.push(format!("{id}: {e}")),
            }
        }
        if !failures.is_empty() {
            return Err(Error::Fetch(failures));
        }

        let rendered = self
            .plan
            .files
            .iter()
            .map(|f| Ok((f, output::render(f, &secrets)?)))
            .collect::<Result<Vec<_>, Error>>()?;

        let mut written = Vec::new();
        for (file, contents) in rendered {
            if output::write_file(&file.path, &contents, self.write_opts)? {
                written.push(file.path.display().to_string());
            }
        }

        self.versions = secrets.into_iter().map(|(id, s)| (id, s.version)).collect();
        log::info(
            "Secrets written",
            json!({
                "updated_files": written,
                "versions": self.versions.iter().collect::<BTreeMap<_, _>>(),
            }),
        );
        Ok(())
    }

    /// Check whether any secret has a new version and re-sync if so.
    ///
    /// A secret whose version can't be checked right now is treated as
    /// unchanged (and logged); it is checked again on the next call.
    pub async fn check(&mut self) -> Result<CheckOutcome, Error> {
        let ids = self.plan.secret_ids();
        let results = join_all(ids.iter().map(|id| self.source.current_version(id))).await;

        let mut changed = BTreeMap::new();
        for (id, result) in ids.iter().zip(results) {
            match result {
                Ok(version) => {
                    let previous = self.versions.get(id);
                    if previous != Some(&version) {
                        changed.insert(id.as_str(), json!({"from": previous, "to": version}));
                    }
                }
                Err(e) => log::warn(
                    "Failed to check secret version",
                    json!({"secret_id": id, "error": e.to_string()}),
                ),
            }
        }

        if changed.is_empty() {
            return Ok(CheckOutcome::Unchanged);
        }
        log::info("Secret versions changed", json!({"changed": changed}));
        self.sync().await?;
        Ok(CheckOutcome::Updated)
    }

    /// Initial sync, then (unless `oneshot`) check for changes every
    /// `interval`, forever. Only the initial sync is fatal: later failures
    /// are logged and the previous files are kept until a retry succeeds.
    pub async fn run(mut self, interval: Duration, oneshot: bool) -> Result<(), Error> {
        self.sync().await?;
        if oneshot {
            return Ok(());
        }
        log::info(
            "Watching for secret changes",
            json!({"interval_seconds": interval.as_secs()}),
        );
        loop {
            tokio::time::sleep(interval).await;
            if let Err(e) = self.check().await {
                log::error(
                    "Failed to update secrets; keeping previous files",
                    json!({"error": e.to_string()}),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::source::{FetchedSecret, SourceError};
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Fake {
        /// id -> (version, data)
        secrets: Mutex<HashMap<String, (String, Vec<u8>)>>,
        fail_access: Mutex<bool>,
        fail_version: Mutex<bool>,
        access_calls: AtomicUsize,
    }

    impl Fake {
        fn set(&self, id: &str, version: &str, data: &str) {
            self.secrets
                .lock()
                .unwrap()
                .insert(id.into(), (version.into(), data.into()));
        }
    }

    impl SecretSource for &Fake {
        async fn access(&self, id: &str) -> Result<FetchedSecret, SourceError> {
            self.access_calls.fetch_add(1, Ordering::SeqCst);
            if *self.fail_access.lock().unwrap() {
                return Err("boom".into());
            }
            let secrets = self.secrets.lock().unwrap();
            let (version, data) = secrets.get(id).ok_or("not found")?;
            Ok(FetchedSecret {
                version: version.clone(),
                data: data.clone(),
            })
        }

        async fn current_version(&self, id: &str) -> Result<String, SourceError> {
            if *self.fail_version.lock().unwrap() {
                return Err("boom".into());
            }
            let secrets = self.secrets.lock().unwrap();
            Ok(secrets.get(id).ok_or("not found")?.0.clone())
        }
    }

    const A: &str = "projects/p/secrets/a/versions/latest";
    const B: &str = "projects/p/secrets/b/versions/latest";

    struct Env {
        dir: tempfile::TempDir,
        plan: Plan,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let yaml = format!(
                "secrets:
  - {{env_name: A, secret_id: {A}}}
  - {{env_name: B, secret_id: {B}}}
  - {{file_name: raw/b.txt, secret_id: {B}}}
"
            );
            let plan = config::parse(
                &yaml,
                &dir.path().join("out.env"),
                &dir.path().join("out.d"),
            )
            .unwrap();
            Self { dir, plan }
        }

        fn read(&self, rel: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(rel)).unwrap()
        }

        fn exists(&self, rel: &str) -> bool {
            Path::new(&self.dir.path().join(rel)).exists()
        }
    }

    #[tokio::test]
    async fn sync_writes_all_files() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "4", "beta\n");
        let mut s = Syncer::new(&fake, env.plan.clone(), WriteOptions::default());
        s.sync().await.unwrap();

        assert_eq!(env.read("out.env"), "A=alpha\nB=beta\n");
        assert_eq!(env.read("out.d/raw/b.txt"), "beta\n");
        // B is used twice but fetched once.
        assert_eq!(fake.access_calls.load(Ordering::SeqCst), 2);
        assert_eq!(s.versions()[A], "1");
        assert_eq!(s.versions()[B], "4");
    }

    #[tokio::test]
    async fn sync_writes_nothing_if_any_fetch_fails() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha"); // B missing
        let mut s = Syncer::new(&fake, env.plan.clone(), WriteOptions::default());
        let err = s.sync().await.unwrap_err();
        assert!(err.to_string().contains(B), "{err}");
        assert!(!env.exists("out.env"));
        assert!(!env.exists("out.d"));
    }

    #[tokio::test]
    async fn sync_writes_nothing_if_any_value_is_invalid() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "1", "multi\nline");
        let mut s = Syncer::new(&fake, env.plan.clone(), WriteOptions::default());
        assert!(matches!(
            s.sync().await.unwrap_err(),
            Error::InvalidEnvValue { .. }
        ));
        assert!(!env.exists("out.d"));
    }

    #[tokio::test]
    async fn check_does_nothing_when_versions_unchanged() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "1", "beta");
        let mut s = Syncer::new(&fake, env.plan.clone(), WriteOptions::default());
        s.sync().await.unwrap();
        assert_eq!(s.check().await.unwrap(), CheckOutcome::Unchanged);
        assert_eq!(fake.access_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn check_rewrites_files_on_new_version() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "1", "beta");
        let mut s = Syncer::new(&fake, env.plan.clone(), WriteOptions::default());
        s.sync().await.unwrap();

        fake.set(B, "2", "beta2");
        assert_eq!(s.check().await.unwrap(), CheckOutcome::Updated);
        assert_eq!(env.read("out.env"), "A=alpha\nB=beta2\n");
        assert_eq!(env.read("out.d/raw/b.txt"), "beta2");
        assert_eq!(s.versions()[B], "2");
        assert_eq!(s.check().await.unwrap(), CheckOutcome::Unchanged);
    }

    #[tokio::test]
    async fn check_ignores_version_lookup_failures() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "1", "beta");
        let mut s = Syncer::new(&fake, env.plan.clone(), WriteOptions::default());
        s.sync().await.unwrap();

        *fake.fail_version.lock().unwrap() = true;
        fake.set(B, "2", "beta2");
        assert_eq!(s.check().await.unwrap(), CheckOutcome::Unchanged);
        assert_eq!(env.read("out.env"), "A=alpha\nB=beta\n");

        *fake.fail_version.lock().unwrap() = false;
        assert_eq!(s.check().await.unwrap(), CheckOutcome::Updated);
        assert_eq!(env.read("out.env"), "A=alpha\nB=beta2\n");
    }

    #[tokio::test]
    async fn failed_refresh_keeps_old_files_and_retries() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "1", "beta");
        let mut s = Syncer::new(&fake, env.plan.clone(), WriteOptions::default());
        s.sync().await.unwrap();

        fake.set(A, "2", "alpha2");
        *fake.fail_access.lock().unwrap() = true;
        assert!(s.check().await.is_err());
        assert_eq!(env.read("out.env"), "A=alpha\nB=beta\n");
        assert_eq!(s.versions()[A], "1");

        *fake.fail_access.lock().unwrap() = false;
        assert_eq!(s.check().await.unwrap(), CheckOutcome::Updated);
        assert_eq!(env.read("out.env"), "A=alpha2\nB=beta\n");
    }

    #[tokio::test]
    async fn run_oneshot_returns_after_first_sync() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "1", "beta");
        Syncer::new(&fake, env.plan.clone(), WriteOptions::default())
            .run(Duration::from_secs(3600), true)
            .await
            .unwrap();
        assert_eq!(env.read("out.env"), "A=alpha\nB=beta\n");
    }

    #[tokio::test]
    async fn run_fails_if_initial_sync_fails() {
        let env = Env::new();
        let fake = Fake::default();
        assert!(
            Syncer::new(&fake, env.plan.clone(), WriteOptions::default())
                .run(Duration::from_secs(1), false)
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_picks_up_changes_on_each_interval() {
        let env = Env::new();
        let fake = Fake::default();
        fake.set(A, "1", "alpha");
        fake.set(B, "1", "beta");
        let fake: &'static Fake = Box::leak(Box::new(fake));
        let plan = env.plan.clone();
        let task = tokio::spawn(async move {
            Syncer::new(fake, plan, WriteOptions::default())
                .run(Duration::from_secs(60), false)
                .await
        });

        // Let the initial sync happen.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(env.read("out.env"), "A=alpha\nB=beta\n");

        fake.set(A, "2", "alpha2");
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(
            env.read("out.env"),
            "A=alpha\nB=beta\n",
            "updated too early"
        );
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert_eq!(env.read("out.env"), "A=alpha2\nB=beta\n");

        assert!(!task.is_finished());
        task.abort();
    }
}
