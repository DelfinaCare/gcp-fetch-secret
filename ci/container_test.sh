#!/usr/bin/env bash
# Integration test for the built container image against a fake Secret
# Manager. Usage: ci/container_test.sh IMAGE
set -euo pipefail

IMAGE=$1
PORT=18080
WORK=$(mktemp -d)
cleanup() {
  docker rm -f gcp-fetch-secret-test >/dev/null 2>&1 || true
  kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*"; docker logs gcp-fetch-secret-test 2>&1 || true; exit 1; }

cat > "$WORK/secrets.json" <<'JSON'
{"projects/p/secrets/db": {"version": 1, "data": "hunter2\n"},
 "projects/p/secrets/key": {"version": 4, "data": "-----BEGIN KEY-----\n"}}
JSON
# Run as an unprivileged user so permission handling is really exercised
# (root ignores the read-only bit on the files being rewritten).
mkdir -p "$WORK/out"
chmod 755 "$WORK"
chmod 777 "$WORK/out"
AS_USER=65534:65534
cat > "$WORK/in.yaml" <<'YAML'
secrets:
  - env_name: DB_PASSWORD
    secret_id: projects/p/secrets/db/versions/latest
  - file_name: key.pem
    secret_id: projects/p/secrets/key/versions/latest
YAML

python3 "$(dirname "$0")/fake_secret_manager.py" "$PORT" "$WORK/secrets.json" &
FAKE_PID=$!
sleep 1

run() {
  docker run --rm --network host -v "$WORK:/work" --user "$AS_USER" \
    "$IMAGE" /work/in.yaml /work/out/secrets.env /work/out/files \
    --endpoint "http://127.0.0.1:$PORT" "$@"
}

# Binary runs in the scratch image.
docker run --rm "$IMAGE" --help >/dev/null || fail "--help"

# Bad config exits 2.
set +e
docker run --rm -v "$WORK:/work" "$IMAGE" /work/secrets.json /work/o /work/d --oneshot
[ $? -eq 2 ] || fail "invalid config should exit 2"
set -e

# One-shot fetch.
run --oneshot || fail "oneshot run"
[ "$(cat "$WORK/out/secrets.env")" = "DB_PASSWORD=hunter2" ] || fail "env file: $(cat "$WORK/out/secrets.env")"
[ "$(cat "$WORK/out/files/key.pem")" = "-----BEGIN KEY-----" ] || fail "key.pem"
[ "$(stat -c %a "$WORK/out/secrets.env")" = "444" ] || fail "mode"

# Watch mode picks up a rotated secret.
docker run -d --name gcp-fetch-secret-test --network host -v "$WORK:/work" --user "$AS_USER" \
  "$IMAGE" /work/in.yaml /work/out/secrets.env /work/out/files \
  --endpoint "http://127.0.0.1:$PORT" --sleep 1 >/dev/null
INODE=$(stat -c %i "$WORK/out/secrets.env")
sed -i 's/"version": 1, "data": "hunter2\\n"/"version": 2, "data": "rotated"/' "$WORK/secrets.json"
for i in $(seq 1 20); do
  [ "$(cat "$WORK/out/secrets.env")" = "DB_PASSWORD=rotated" ] && break
  [ "$i" -eq 20 ] && fail "rotation not picked up"
  sleep 0.5
done

# Updated in place (same inode), so single-file bind mounts see the change.
[ "$(stat -c %i "$WORK/out/secrets.env")" = "$INODE" ] || fail "file was replaced, not updated in place"
[ "$(stat -c %a "$WORK/out/secrets.env")" = "444" ] || fail "mode after update"

# SIGTERM exits cleanly.
docker stop gcp-fetch-secret-test >/dev/null
[ "$(docker inspect -f '{{.State.ExitCode}}' gcp-fetch-secret-test)" = "0" ] || fail "exit code after SIGTERM"

echo "All container tests passed"
