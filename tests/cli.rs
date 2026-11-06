//! End-to-end tests: run the real binary against a fake Secret Manager REST
//! API, exercising the actual SDK client (URL paths, JSON, base64 payloads,
//! error statuses).

use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde_json::json;
use tokio::process::{Child, Command};

/// secret name (`projects/p/secrets/s`) -> (version number, payload)
type Secrets = Arc<Mutex<HashMap<String, (u32, Vec<u8>)>>>;

async fn handle(State(secrets): State<Secrets>, UrlPath(path): UrlPath<String>) -> Response {
    let (path, access) = match path.strip_suffix(":access") {
        Some(p) => (p, true),
        None => (path.as_str(), false),
    };
    let Some((secret, _requested_version)) = path.rsplit_once("/versions/") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some((version, data)) = secrets.lock().unwrap().get(secret).cloned() else {
        let body = json!({"error": {"code": 404, "message": format!("Secret [{secret}] not found"), "status": "NOT_FOUND"}});
        return (StatusCode::NOT_FOUND, Json(body)).into_response();
    };
    // The real API answers with the project *number* and resolved version.
    let name = format!(
        "{}/versions/{version}",
        secret.replacen("projects/p/", "projects/1234/", 1)
    );
    let body = if access {
        json!({"name": name, "payload": {"data": base64::engine::general_purpose::STANDARD.encode(data)}})
    } else {
        json!({"name": name, "state": "ENABLED"})
    };
    Json(body).into_response()
}

async fn fake_secret_manager(secrets: Secrets) -> String {
    let app = axum::Router::new()
        .route("/v1/{*path}", axum::routing::get(handle))
        .with_state(secrets);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn set(secrets: &Secrets, name: &str, version: u32, data: &[u8]) {
    secrets
        .lock()
        .unwrap()
        .insert(name.into(), (version, data.to_vec()));
}

const CONFIG: &str = "secrets:
  - env_name: DB_PASSWORD
    secret_id: projects/p/secrets/db/versions/latest
  - env_name: API_KEY
    secret_id: projects/p/secrets/api/versions/latest
  - file_name: certs/key.pem
    secret_id: projects/p/secrets/key/versions/latest
";

fn spawn(dir: &Path, endpoint: &str, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_secretsfetch"))
        .arg(dir.join("in.yaml"))
        .arg(dir.join("out.env"))
        .arg(dir.join("out.d"))
        .args(["--endpoint", endpoint])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn finish(child: Child) -> (ExitStatus, String) {
    let out = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("secretsfetch did not exit")
        .unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status, log)
}

fn setup() -> (tempfile::TempDir, Secrets) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("in.yaml"), CONFIG).unwrap();
    let secrets = Secrets::default();
    set(&secrets, "projects/p/secrets/db", 3, b"hunter2\n");
    set(&secrets, "projects/p/secrets/api", 1, b"abc123");
    set(
        &secrets,
        "projects/p/secrets/key",
        7,
        b"-----BEGIN-----\n\x00\xff\n",
    );
    (dir, secrets)
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[tokio::test(flavor = "multi_thread")]
async fn oneshot_writes_files() {
    let (dir, secrets) = setup();
    let endpoint = fake_secret_manager(secrets).await;
    let (status, log) = finish(spawn(dir.path(), &endpoint, &["--oneshot"])).await;
    assert!(status.success(), "{status}\n{log}");

    let env_file = dir.path().join("out.env");
    assert_eq!(
        std::fs::read_to_string(&env_file).unwrap(),
        "DB_PASSWORD=hunter2\nAPI_KEY=abc123\n"
    );
    assert_eq!(mode(&env_file), 0o444);
    let key = dir.path().join("out.d/certs/key.pem");
    assert_eq!(std::fs::read(&key).unwrap(), b"-----BEGIN-----\n\x00\xff\n");
    assert_eq!(mode(&key), 0o444);

    // Logs are JSON lines and never contain secret values.
    for line in log.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|_| panic!("{line}"));
    }
    assert!(!log.contains("hunter2") && !log.contains("abc123"), "{log}");
    assert!(
        log.contains(r#""projects/p/secrets/db/versions/latest":"3""#),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_file_mode() {
    let (dir, secrets) = setup();
    let endpoint = fake_secret_manager(secrets).await;
    let (status, log) = finish(spawn(
        dir.path(),
        &endpoint,
        &["--oneshot", "--file-mode", "640"],
    ))
    .await;
    assert!(status.success(), "{log}");
    assert_eq!(mode(&dir.path().join("out.env")), 0o640);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_mode_updates_files_and_exits_cleanly_on_sigterm() {
    let (dir, secrets) = setup();
    let endpoint = fake_secret_manager(secrets.clone()).await;
    let child = spawn(dir.path(), &endpoint, &["--sleep", "1"]);
    let env_file = dir.path().join("out.env");

    let wait_for = |expected: &'static str| {
        let env_file = env_file.clone();
        async move {
            for _ in 0..100 {
                if std::fs::read_to_string(&env_file).is_ok_and(|s| s == expected) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!(
                "never saw {expected:?}, have {:?}",
                std::fs::read_to_string(&env_file)
            );
        }
    };

    wait_for("DB_PASSWORD=hunter2\nAPI_KEY=abc123\n").await;
    let inode = std::fs::metadata(&env_file).unwrap().ino();
    set(&secrets, "projects/p/secrets/api", 2, b"rotated");
    wait_for("DB_PASSWORD=hunter2\nAPI_KEY=rotated\n").await;
    // Updated in place by default, so a bind mount of the file sees it.
    assert_eq!(std::fs::metadata(&env_file).unwrap().ino(), inode);
    assert_eq!(mode(&env_file), 0o444);

    let pid = child.id().unwrap().to_string();
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let (status, log) = finish(child).await;
    assert!(status.success(), "{status}\n{log}");
    assert!(log.contains("Shutdown signal received"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_secret_exits_2_without_writing() {
    let (dir, secrets) = setup();
    secrets.lock().unwrap().remove("projects/p/secrets/key");
    let endpoint = fake_secret_manager(secrets).await;
    let (status, log) = finish(spawn(dir.path(), &endpoint, &["--oneshot"])).await;
    assert_eq!(status.code(), Some(2), "{log}");
    assert!(
        log.contains("projects/p/secrets/key/versions/latest"),
        "{log}"
    );
    assert!(!dir.path().join("out.env").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_exits_2() {
    let (dir, _) = setup();
    std::fs::write(
        dir.path().join("in.yaml"),
        "secrets:\n  - secret_id: nope\n",
    )
    .unwrap();
    let (status, log) = finish(spawn(dir.path(), "http://127.0.0.1:1", &["--oneshot"])).await;
    assert_eq!(status.code(), Some(2), "{log}");
    assert!(log.contains("secrets[0].secret_id"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_config_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let (status, log) = finish(spawn(dir.path(), "http://127.0.0.1:1", &["--oneshot"])).await;
    assert_eq!(status.code(), Some(2), "{log}");
    assert!(log.contains("failed to read config"), "{log}");
}
