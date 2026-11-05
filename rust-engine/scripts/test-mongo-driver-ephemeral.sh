#!/usr/bin/env bash
set -euo pipefail

command -v mongod >/dev/null || { echo "mongod is required; no server will be installed" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required to reserve a loopback port" >&2; exit 1; }
temp_root="${TMPDIR:-/tmp}"
[[ -d "$temp_root" ]] || { echo "Temporary parent does not exist" >&2; exit 1; }
run_dir="$(mktemp -d "${temp_root%/}/tablex-mongo-ephemeral.XXXXXXXX")"
owner="$(uuidgen)"
owner="ephem_${owner//-/}"
port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
[[ "$port" != 27017 && "$port" -gt 1024 ]] || { echo "Refusing default/privileged MongoDB port" >&2; exit 1; }

cleanup() {
    if [[ "${owned_pid:-}" =~ ^[0-9]+$ ]]; then
        pid="$owned_pid"
        if [[ "$pid" =~ ^[0-9]+$ ]]; then
            command_line="$(ps -p "$pid" -o command= 2>/dev/null || true)"
            if [[ "$command_line" == *mongod* && "$command_line" == *"$run_dir/data"* ]]; then
                kill -TERM "$pid" 2>/dev/null || true
                for _ in {1..50}; do
                    kill -0 "$pid" 2>/dev/null || break
                    sleep 0.1
                done
                if kill -0 "$pid" 2>/dev/null; then
                    echo "Owned mongod $pid is still running; retaining $run_dir for manual inspection" >&2
                    return
                fi
            fi
        fi
    fi
    [[ "$run_dir" == *"/tablex-mongo-ephemeral."* ]] && rm -rf -- "$run_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$run_dir/data"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bash "$script_dir/build-mongo-driver.sh" --target-dir "$run_dir/driver-target"
driver="$run_dir/driver-target/release/libdbengine_mongo.dylib"
[[ -f "$driver" ]] || { echo "Isolated current Mongo driver build is missing" >&2; exit 1; }
mongod --dbpath "$run_dir/data" --bind_ip 127.0.0.1 --port "$port" \
    --nounixsocket --logpath "$run_dir/mongod.log" --pidfilepath "$run_dir/mongod.pid" &
owned_pid=$!
for _ in {1..100}; do
    [[ -f "$run_dir/mongod.pid" ]] && break
    kill -0 "$owned_pid" 2>/dev/null || { echo "Owned mongod exited before startup" >&2; exit 1; }
    sleep 0.1
done
[[ -f "$run_dir/mongod.pid" ]] || { echo "Owned mongod startup timed out" >&2; exit 1; }

TABLEX_EPHEMERAL_MONGO_URI="mongodb://127.0.0.1:${port}/?directConnection=true&serverSelectionTimeoutMS=2000" \
TABLEX_EPHEMERAL_MONGO_OWNER="$owner" \
TABLEX_EPHEMERAL_MONGO_PIDFILE="$run_dir/mongod.pid" \
TABLEX_EPHEMERAL_MONGO_DRIVER="$driver" \
"${CARGO:-cargo}" test --manifest-path "$script_dir/../Cargo.toml" \
    -p engine-driver-host --test loader ephemeral_mongo_script_create_enumerate_query_and_release \
    -- --ignored --exact --nocapture

TABLEX_EPHEMERAL_MONGO_URI="mongodb://127.0.0.1:${port}/?directConnection=true&serverSelectionTimeoutMS=2000" \
TABLEX_EPHEMERAL_MONGO_OWNER="$owner" \
TABLEX_EPHEMERAL_MONGO_PIDFILE="$run_dir/mongod.pid" \
TABLEX_EPHEMERAL_MONGO_DRIVER="$driver" \
"${CARGO:-cargo}" test --manifest-path "$script_dir/../../Cargo.toml" \
    -p tablex-gpui --bin tablex-gpui app::tests::ephemeral_mongo_filter_hides_server_database_without_deleting_cached_node \
    -- --ignored --exact --nocapture

TABLEX_EPHEMERAL_MONGO_URI="mongodb://127.0.0.1:${port}/?directConnection=true&serverSelectionTimeoutMS=2000" \
TABLEX_EPHEMERAL_MONGO_OWNER="$owner" \
TABLEX_EPHEMERAL_MONGO_PIDFILE="$run_dir/mongod.pid" \
TABLEX_EPHEMERAL_MONGO_DRIVER="$driver" \
"${CARGO:-cargo}" test --manifest-path "$script_dir/../../Cargo.toml" \
    -p tablex-gpui --bin tablex-gpui app::tests::ephemeral_mongo_create_database_from_the_menu_lists_it_behind_a_filter \
    -- --ignored --exact --nocapture
