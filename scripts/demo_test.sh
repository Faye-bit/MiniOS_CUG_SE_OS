#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVER_BIN="$ROOT_DIR/target/release/minios-server"
CLIENT_BIN="$ROOT_DIR/target/release/minios-client"

WORK_DIR="${MINIOS_DEMO_DIR:-/tmp/minios_demo}"
STORE_PATH="$WORK_DIR/demo_store.odb"
SOCKET_PATH="$WORK_DIR/minios.sock"
SHM_NAME="${MINIOS_DEMO_SHM:-/minios_demo_shm}"
PID_FILE="$WORK_DIR/minios.pid"
LOG_FILE="$WORK_DIR/minios.log"

SMALL_FILE="$WORK_DIR/hello.txt"
META_FILE="$WORK_DIR/meta.json"
EMPTY_FILE="$WORK_DIR/empty.bin"
LARGE_FILE="$WORK_DIR/large.bin"
DOWNLOAD_NAME="$WORK_DIR/download_name.txt"
DOWNLOAD_UUID="$WORK_DIR/download_uuid.txt"
DOWNLOAD_LARGE="$WORK_DIR/download_large.bin"

SERVER_PID=""

pass() {
    printf '[PASS] %s\n' "$1"
}

fail() {
    printf '[FAIL] %s\n' "$1" >&2
    if [[ -f "$LOG_FILE" ]]; then
        printf '\n--- minios server log ---\n' >&2
        tail -80 "$LOG_FILE" >&2 || true
    fi
    exit 1
}

client() {
    "$CLIENT_BIN" --socket "$SOCKET_PATH" --shm-name "$SHM_NAME" "$@"
}

cleanup() {
    set +e
    if [[ -S "$SOCKET_PATH" ]]; then
        client stop >/dev/null 2>&1
    fi
    if [[ -n "${SERVER_PID:-}" ]]; then
        wait "$SERVER_PID" 2>/dev/null
    fi
}
trap cleanup EXIT

wait_for_server() {
    for _ in $(seq 1 50); do
        if client status >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    fail "server did not become ready"
}

require_file_contains() {
    local file="$1"
    local pattern="$2"
    grep -q "$pattern" "$file" || fail "expected '$pattern' in $file"
}

require_cmd_contains() {
    local pattern="$1"
    shift
    "$@" | grep -q "$pattern" || fail "expected command output to contain '$pattern': $*"
}

printf '== MiniOS demo test ==\n'
printf 'workspace: %s\n' "$ROOT_DIR"
printf 'work dir : %s\n' "$WORK_DIR"

mkdir -p "$WORK_DIR"
rm -f "$STORE_PATH" "$SOCKET_PATH" "$PID_FILE" "$LOG_FILE" \
    "$SMALL_FILE" "$META_FILE" "$EMPTY_FILE" "$LARGE_FILE" \
    "$DOWNLOAD_NAME" "$DOWNLOAD_UUID" "$DOWNLOAD_LARGE" \
    "$WORK_DIR"/concurrent_*.txt

printf '\n== Build release binaries ==\n'
cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml" >/dev/null
[[ -x "$SERVER_BIN" ]] || fail "missing server binary: $SERVER_BIN"
[[ -x "$CLIENT_BIN" ]] || fail "missing client binary: $CLIENT_BIN"
pass "release build"

printf '\n== Start server through client ==\n'
START_OUTPUT="$(client start \
    --server "$SERVER_BIN" \
    --store-path "$STORE_PATH" \
    --log-file "$LOG_FILE")"
printf '%s\n' "$START_OUTPUT"
SERVER_PID="$(printf '%s\n' "$START_OUTPUT" | sed -n 's/.*pid=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$SERVER_PID" ]] || fail "cannot parse server pid from start output"
wait_for_server
pass "server start/status"

printf '\n== Basic put/list/get ==\n'
printf 'Hello, MiniOS! This is a demo file.\n' > "$SMALL_FILE"
PUT_OUTPUT="$(client put "$SMALL_FILE" --name hello-world --content-type text/plain)"
printf '%s\n' "$PUT_OUTPUT"
UUID_HELLO="$(printf '%s\n' "$PUT_OUTPUT" | awk '/^OK / {print $2}')"
[[ "$UUID_HELLO" =~ ^[0-9a-fA-F]{32}$ ]] || fail "put did not return a 32-hex UUID"

client list > "$WORK_DIR/list_1.txt"
require_file_contains "$WORK_DIR/list_1.txt" "hello-world"
client get hello-world --output "$DOWNLOAD_NAME" >/dev/null
cmp "$SMALL_FILE" "$DOWNLOAD_NAME" || fail "download by name differs from source"
client get "$UUID_HELLO" --output "$DOWNLOAD_UUID" >/dev/null
cmp "$SMALL_FILE" "$DOWNLOAD_UUID" || fail "download by UUID differs from source"
pass "put/list/get by name and UUID"

printf '\n== Tags, status, and cache visibility ==\n'
printf '{"author":"Alice","project":"demo"}\n' > "$META_FILE"
client put "$META_FILE" \
    --name config \
    --content-type application/json \
    --tags '{"author":"Alice","project":"demo"}' >/dev/null
client get hello-world >/dev/null
client get hello-world >/dev/null
STATUS_OUTPUT="$(client status)"
printf '%s\n' "$STATUS_OUTPUT"
printf '%s\n' "$STATUS_OUTPUT" | grep -q 'cache_hit_rate:' || fail "status missing cache_hit_rate"
printf '%s\n' "$STATUS_OUTPUT" | grep -q 'shm_pages_free:' || fail "status missing shm_pages_free"
pass "tags/status/cache fields"

printf '\n== Empty object upload ==\n'
: > "$EMPTY_FILE"
client put "$EMPTY_FILE" --name empty-object --content-type application/octet-stream >/dev/null
EMPTY_GET_OUTPUT="$(client get empty-object 2>&1 || true)"
printf '%s\n' "$EMPTY_GET_OUTPUT" | grep -q 'OK (empty object)' || fail "empty object get did not report empty object"
pass "empty object"

printf '\n== Large object chunked upload ==\n'
dd if=/dev/urandom of="$LARGE_FILE" bs=1M count=2 status=none
client put "$LARGE_FILE" --name large-test --content-type application/octet-stream >/dev/null
client get large-test --output "$DOWNLOAD_LARGE" >/dev/null
cmp "$LARGE_FILE" "$DOWNLOAD_LARGE" || fail "large object roundtrip differs from source"
pass "large chunked upload/download"

printf '\n== Restart persistence and cache warmup path ==\n'
client stop >/dev/null
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""

START_OUTPUT="$(client start \
    --server "$SERVER_BIN" \
    --store-path "$STORE_PATH" \
    --log-file "$LOG_FILE")"
printf '%s\n' "$START_OUTPUT"
SERVER_PID="$(printf '%s\n' "$START_OUTPUT" | sed -n 's/.*pid=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$SERVER_PID" ]] || fail "cannot parse restarted server pid"
wait_for_server
require_cmd_contains "hello-world" client list
client get hello-world --output "$DOWNLOAD_NAME" >/dev/null
cmp "$SMALL_FILE" "$DOWNLOAD_NAME" || fail "persistent object differs after restart"
pass "restart persistence"

printf '\n== Concurrent uploads ==\n'
pids=()
for i in $(seq 1 10); do
    printf 'concurrent-data-%s\n' "$i" > "$WORK_DIR/concurrent_$i.txt"
    client put "$WORK_DIR/concurrent_$i.txt" --name "concurrent-$i" >/dev/null &
    pids+=("$!")
done
for pid in "${pids[@]}"; do
    wait "$pid"
done
CONCURRENT_COUNT="$(client list | grep -c 'concurrent-')"
[[ "$CONCURRENT_COUNT" -eq 10 ]] || fail "expected 10 concurrent objects, got $CONCURRENT_COUNT"
pass "concurrent uploads"

printf '\n== Delete object ==\n'
UUID_CONFIG="$(client list | awk '$2 == "config" {print $1; exit}')"
[[ -n "$UUID_CONFIG" ]] || fail "cannot find config UUID"
client delete "$UUID_CONFIG" >/dev/null
if client list | grep -q ' config '; then
    fail "config object still appears after delete"
fi
pass "delete"

printf '\n== Stop server ==\n'
client stop >/dev/null
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
pass "stop"

printf '\nAll MiniOS demo checks passed.\n'
