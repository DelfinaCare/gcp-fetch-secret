# gcp-fetch-secret

Fetches secrets from [GCP Secret Manager](https://cloud.google.com/secret-manager)
into env files and secret files for bootstrapping programs on non-Cloud Run
services (e.g. docker containers on GCE), and keeps them up to date as new
secret versions are added.

## Design Goals

- **Minimal attack surface**: written in Rust and shipped in a `scratch` image
  that holds only the static binary and a CA bundle (~8 MB).
- **Safe file handling**: new files are created atomically and are private
  (`0600`) until fully written; all files end up read-only (`0444` by
  default). Existing files are updated in place, so containers that
  bind-mount a single file see changes. Symlinks are never written through.
- **Fail fast on bad config**: the whole YAML file is validated before
  anything is fetched, and every problem is reported at once.

## Usage

```
gcp-fetch-secret [OPTIONS] <INPUT_FILE> <OUTPUT_FILE> <OUTPUT_DIR>
```

| Argument / flag       | Description                                                                        |
|-----------------------|------------------------------------------------------------------------------------|
| `INPUT_FILE`          | YAML file listing the secrets to fetch                                             |
| `OUTPUT_FILE`         | Where to write secrets that have no `file_name`                                    |
| `OUTPUT_DIR`          | Directory for secrets that have a `file_name`                                      |
| `-s`, `--sleep <N>`   | Seconds between checks for new versions (default `60`)                             |
| `-o`, `--oneshot`     | Write the files once and exit                                                      |
| `--file-mode <MODE>`  | Octal permission bits for written files (default `444`)                            |
| `--write-mode <MODE>` | `in-place` (default) or `replace`; see [Updating files](#updating-files)          |
| `--endpoint <URL>`    | Secret Manager endpoint override, e.g. a regional endpoint. Also `GCP_FETCH_SECRET_ENDPOINT` |

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
  ghcr.io/delfinacare/gcp-fetch-secret:latest /in.yaml /secrets/app.env /secrets/files
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

Every `--sleep` seconds, gcp-fetch-secret asks Secret Manager which version each
`secret_id` resolves to (metadata only, no payload). If any changed, all
secrets are fetched again and the files are rewritten. Files whose content
didn't change are left alone. A secret referenced by several entries is
fetched once.

### Updating files

A file that doesn't exist yet is always created atomically: written to a
private temporary file, then renamed into place.

How an existing file is updated depends on `--write-mode`:

- **`in-place`** (default): the existing file is rewritten (same inode), the
  way the Python version did it. Containers that bind-mount a single file
  (`-v /host/secrets/app.env:/app.env`) see the new content. A process
  reading at the exact moment of the update can see a truncated file. The
  file is made owner-writable only for the duration of the write, and must
  be a regular file owned by the user gcp-fetch-secret runs as (or it must run
  as root); symlinks are refused.
- **`replace`**: a new file is renamed over the old one, so readers only ever
  see the complete old or new file. A single-file bind mount keeps showing
  the old file, though; bind-mount the directory instead.

Note that `docker run --env-file` reads the env file once, when the container
starts, in either mode.

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
docker build -t gcp-fetch-secret:test .
ci/container_test.sh gcp-fetch-secret:test
```

`cargo test` includes end-to-end tests (`tests/cli.rs`) that run the binary
against an in-process fake of the Secret Manager REST API. Any `http://`
`--endpoint` is used without credentials; it exists for those tests and local
emulators, so an access token is never sent over plain HTTP.

## CI/CD

GitHub Actions workflows live in `.github/workflows/`:

- **`ci.yml`** (pushes and PRs to `main`): `cargo fmt --check`, clippy, `cargo
  test`, actionlint on the workflows, and a Docker build on native amd64 and
  arm64 runners followed by `ci/container_test.sh` against each image.
- **`release.yml`** (on publishing a GitHub release): checks the tag matches
  the `Cargo.toml` version, builds the image on native amd64/arm64 runners,
  merges them into one multi-arch image at
  `ghcr.io/delfinacare/gcp-fetch-secret` tagged `X.Y.Z`, `X.Y` and `X`, attaches
  build provenance attestations and an SBOM, and uploads static
  `gcp-fetch-secret-linux-{amd64,arm64}` binaries to the release.

Dependabot (`.github/dependabot.yml`) updates crates, actions and the base
image weekly, with the Google Cloud crates grouped into one PR.

### Releasing

1. Bump `version` in `Cargo.toml`, run `cargo check` to update `Cargo.lock`,
   and merge that to `main`.
2. Create a GitHub release with tag `vX.Y.Z` matching that version.

Verify a downloaded artifact with:

```bash
gh attestation verify oci://ghcr.io/delfinacare/gcp-fetch-secret:X.Y.Z --owner DelfinaCare
gh attestation verify gcp-fetch-secret-linux-amd64 --owner DelfinaCare
```

## License

[MIT](LICENSE)
