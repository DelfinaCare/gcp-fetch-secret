# secretsfetch

Fetches secrets from [GCP Secret Manager](https://cloud.google.com/secret-manager)
into env files and secret files for bootstrapping programs on non-Cloud Run
services (e.g. docker containers on GCE), and keeps them up to date as new
secret versions are added.

## Design Goals

- **Minimal attack surface**: written in Rust and shipped in a `scratch` image
  that holds only the static binary and a CA bundle (~8 MB).
- **Safe file handling**: files are replaced atomically (readers never see a
  partial file), start out private (`0600`) while being written, and end up
  read-only (`0444` by default).
- **Fail fast on bad config**: the whole YAML file is validated before
  anything is fetched, and every problem is reported at once.

## Usage

```
secretsfetch [OPTIONS] <INPUT_FILE> <OUTPUT_FILE> <OUTPUT_DIR>
```

| Argument / flag       | Description                                                                        |
|-----------------------|------------------------------------------------------------------------------------|
| `INPUT_FILE`          | YAML file listing the secrets to fetch                                             |
| `OUTPUT_FILE`         | Where to write secrets that have no `file_name`                                    |
| `OUTPUT_DIR`          | Directory for secrets that have a `file_name`                                      |
| `-s`, `--sleep <N>`   | Seconds between checks for new versions (default `60`)                             |
| `-o`, `--oneshot`     | Write the files once and exit                                                      |
| `--file-mode <MODE>`  | Octal permission bits for written files (default `444`)                            |
| `--endpoint <URL>`    | Secret Manager endpoint override, e.g. a regional endpoint. Also `SECRETSFETCH_ENDPOINT` |

Authentication uses [Application Default Credentials](https://cloud.google.com/docs/authentication/application-default-credentials):
the GCE metadata server, `GOOGLE_APPLICATION_CREDENTIALS`, or gcloud user
credentials. The identity needs `roles/secretmanager.secretAccessor` on each
secret, plus `roles/secretmanager.viewer` (or the
`secretmanager.versions.get` permission) to check for new versions in watch mode.

### Docker

```bash
docker run --rm \
  -v $PWD/in.yaml:/in.yaml:ro \
  -v $PWD/secrets:/secrets \
  ghcr.io/delfinacare/secretsfetch:latest /in.yaml /secrets/app.env /secrets/files
```

### Exit codes

| Code | Meaning                                                           |
|------|-------------------------------------------------------------------|
| 0    | One-shot run finished, or stopped by `SIGTERM` / `SIGINT`         |
| 2    | Invalid config, or the initial fetch/write failed                 |

After the initial sync, failures (API errors, a bad new secret value) are
logged and the previous files are kept; the update is retried on the next
check.

## Configuration

```yaml
secrets:
  # Lines in OUTPUT_FILE
  - env_name: DB_PASSWORD
    secret_id: projects/my-project/secrets/db-password/versions/latest
  - env_name: API_KEY
    secret_id: projects/my-project/secrets/api-key/versions/3

  # Another env file, OUTPUT_DIR/worker.env
  - env_name: QUEUE_TOKEN
    file_name: worker.env
    secret_id: projects/my-project/secrets/queue-token/versions/latest

  # A raw secret file, OUTPUT_DIR/certs/tls.key, holding exactly the secret's bytes
  - file_name: certs/tls.key
    secret_id: projects/my-project/secrets/tls-key/versions/latest
```

Each entry has:

| Field       | Required | Description                                                                                       |
|-------------|----------|---------------------------------------------------------------------------------------------------|
| `secret_id` | yes      | `projects/P/secrets/S/versions/V` (or `projects/P/locations/L/secrets/S/versions/V`). `V` may be `latest`. |
| `env_name`  | no       | Write `ENV_NAME=value` into the file. Without it, the secret is the entire content of its file.  |
| `file_name` | no       | Path relative to `OUTPUT_DIR`. Without it, `OUTPUT_FILE` is used.                                 |

Rules, checked at startup:

- Unknown fields are rejected (so a typo like `envname` is caught).
- A secret without `env_name` must be the only secret written to its file.
- An `env_name` can appear only once per file.
- `file_name` must stay inside `OUTPUT_DIR`: no absolute paths and no `..`.

### Env file format

One `NAME=value` line per entry, compatible with `docker run --env-file`.
Trailing line breaks on a value (as left by `echo value | gcloud secrets
versions add ...`) are dropped. A value with an embedded line break or NUL,
or one that isn't UTF-8, can't be represented in an env file and is an error;
put such secrets in their own file instead.

### Watching for changes

Every `--sleep` seconds, secretsfetch asks Secret Manager which version each
`secret_id` resolves to (metadata only, no payload). If any changed, all
secrets are fetched again and the files are rewritten. Files whose content
didn't change are left alone. A secret referenced by several entries is
fetched once.

Files are replaced by renaming a new file over the old one. A container that
bind-mounts **the directory** sees updates. A container that bind-mounts **a
single file** (`-v /host/secrets/app.env:/app.env`) keeps seeing the file as
it was when the container started, so mount the directory to get live updates.

## Logging

Logs are JSON lines on stdout with `severity` and `message`, which Cloud
Logging parses as structured logs. Secret values are never logged; secret IDs
and version numbers are.

## Development

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test

# Container build + integration test against a fake Secret Manager
docker build -t secretsfetch:test .
ci/container_test.sh secretsfetch:test
```

`cargo test` includes end-to-end tests (`tests/cli.rs`) that run the binary
against an in-process fake of the Secret Manager REST API. Any `http://`
`--endpoint` is used without credentials; it exists for those tests and local
emulators, so an access token is never sent over plain HTTP.

## License

[MIT](LICENSE)
