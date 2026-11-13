#!/usr/bin/env bash
# The release binaries, end to end, through a real TLS proxy: `smoke.sh <directory with the binaries>`.
set -euo pipefail
bin=$(cd "$1" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
proxy_pid="" server_pid="" daemon_a="" daemon_b=""
cleanup() {
    [ -n "$daemon_a" ] && kill "$daemon_a" 2>/dev/null || true
    [ -n "$daemon_b" ] && kill "$daemon_b" 2>/dev/null || true
    [ -n "$proxy_pid" ] && kill "$proxy_pid" 2>/dev/null || true
    [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
    chmod -R u+rwx "$work" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

# This user's own list of roots is left as it was: every `cairn-sync` here keeps to the test's own, and the end
# checks that nothing reached the user's. Release runs once left their roots in it, listed as gone.
user_dir=${CAIRN_USER_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/cairn}
user_names() { if [ -d "$user_dir" ]; then find "$user_dir" -mindepth 1 -printf '%P\n' | LC_ALL=C sort; fi; }
user_names >"$work/user-names"
if [ -f "$user_dir/roots.json" ]; then cp "$user_dir/roots.json" "$work/user-roots.json"; fi
export CAIRN_USER_DIR="$work/user"

# The first line a process prints, once all of it is there: under load that may take long, so this waits
# while the process runs, up to ten minutes, and otherwise fails saying why. A port read before it was
# written once made a URL with none.
first_line() { # <file> <pid> <what> <pattern the line matches>
    local until=$((SECONDS + 600)) line
    until [ -s "$1" ] && [ "$(wc -l <"$1")" -ge 1 ]; do
        if ! kill -0 "$2" 2>/dev/null; then
            echo "smoke test: $3 ended without saying where it listens: $(cat "$1" 2>/dev/null)" >&2
            return 1
        fi
        if [ "$SECONDS" -ge "$until" ]; then
            echo "smoke test: $3 has not said where it listens in ten minutes" >&2
            return 1
        fi
        sleep 0.05
    done
    line=$(head -n 1 "$1")
    if ! [[ $line =~ $4 ]]; then
        echo "smoke test: $3 said \"$line\", not where it listens" >&2
        return 1
    fi
    printf '%s\n' "$line"
}

"$bin/cairn-server" init --data "$work/data" >/dev/null
"$bin/cairn-server" serve --data "$work/data" --listen 127.0.0.1:0 >"$work/server.out" &
server_pid=$!
backend=$(first_line "$work/server.out" "$server_pid" "the server" '^listening on 127\.0\.0\.1:[0-9]+$')
backend=${backend#listening on }

# A private CA and a certificate from it, as a self-hosted proxy would have.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=smoke test CA" \
    -keyout "$work/ca-key.pem" -out "$work/ca.pem" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout "$work/key.pem" -out "$work/cert.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n' >"$work/ext"
openssl x509 -req -in "$work/cert.csr" -CA "$work/ca.pem" -CAkey "$work/ca-key.pem" -CAcreateserial -days 1 \
    -extfile "$work/ext" -out "$work/cert.pem" 2>/dev/null
python3 -I "$here/tls_proxy.py" "$work/cert.pem" "$work/key.pem" "$backend" >"$work/proxy.out" &
proxy_pid=$!
url="https://localhost:$(first_line "$work/proxy.out" "$proxy_pid" "the TLS proxy" '^[0-9]+$')"

export SSL_CERT_FILE="$work/ca.pem"
mkdir "$work/a" "$work/b"
"$bin/cairn-sync" init --server "$url" --token-file "$work/data/token" --device-name a "$work/a" >/dev/null
"$bin/cairn-sync" init --server "$url" --token-file "$work/data/token" --device-name b "$work/b" >/dev/null
# Both listed, in the test's own list.
listed=$("$bin/cairn-sync" status --all --json)
grep -qF "\"$work/a\"" <<<"$listed"
grep -qF "\"$work/b\"" <<<"$listed"

# Metadata and names across two devices.
printf 'hello\n' >"$work/a/hello.txt"
printf '#!/bin/sh\n' >"$work/a/script.sh"; chmod 755 "$work/a/script.sh"
ln -s hello.txt "$work/a/link"
ln "$work/a/hello.txt" "$work/a/hard"
truncate -s 1T "$work/a/sparse"
mkdir "$work/a/node_modules"; echo kept >"$work/a/node_modules/x"
"$bin/cairn-sync" --root "$work/a" sync >/dev/null
"$bin/cairn-sync" --root "$work/b" sync >/dev/null
test "$(cat "$work/b/hello.txt")" = hello
test "$(stat -c %a "$work/b/script.sh")" = 755
test "$(readlink "$work/b/link")" = hello.txt
test "$(stat -c %i "$work/b/hello.txt")" = "$(stat -c %i "$work/b/hard")"
test "$(stat -c %s "$work/b/sparse")" = 1099511627776
test "$(stat -c %b "$work/b/sparse")" -lt 16
test "$(cat "$work/b/node_modules/x")" = kept

# A conflict keeps both, and a restore brings an old version back.
echo from-a >"$work/a/hello.txt"
"$bin/cairn-sync" --root "$work/a" sync >/dev/null
echo from-b >"$work/b/hello.txt"
"$bin/cairn-sync" --root "$work/b" sync >/dev/null
test "$(cat "$work/b/hello.txt")" = from-a
test "$(cat "$work/b/"hello\ \(conflict\ b\ *\).txt)" = from-b
"$bin/cairn-sync" --root "$work/b" conflicts | grep -q " (conflict b [0-9-]*)[^:]*: the competing version of "
"$bin/cairn-sync" --root "$work/a" sync >/dev/null
"$bin/cairn-sync" --root "$work/a" restore hello.txt --at 1 >/dev/null
test "$(cat "$work/a/hello.txt")" = hello

# A folder with no marker pauses.
mkdir "$work/gone"
status=0; "$bin/cairn-sync" --root "$work/gone" sync >"$work/paused.out" || status=$?
test "$status" = 2
grep -q "Sync is paused" "$work/paused.out"

# Daemons keep two roots in step through the proxy, from the watcher and the change feed alone.
"$bin/cairn-sync" --root "$work/b" sync >/dev/null
"$bin/cairn-sync" --root "$work/a" daemon --reconcile-secs 86400 >"$work/daemon-a.out" &
daemon_a=$!
"$bin/cairn-sync" --root "$work/b" daemon --reconcile-secs 86400 >"$work/daemon-b.out" &
daemon_b=$!
wait_for() {
    for _ in $(seq 400); do
        [ "$(cat "$1" 2>/dev/null)" = "$2" ] && return 0
        sleep 0.05
    done
    echo "$1 never held $2" >&2
    return 1
}
sleep 1
echo pushed >"$work/a/from-daemon"
wait_for "$work/b/from-daemon" pushed
echo pulled >"$work/b/from-daemon"
wait_for "$work/a/from-daemon" pulled
kill "$daemon_a" "$daemon_b"; wait "$daemon_a" "$daemon_b" 2>/dev/null || true; daemon_a="" daemon_b=""

# Rules leave a path out, and nothing about it reaches the server.
echo '*.secret' >"$work/a/.cairn/ignore"
echo s >"$work/a/x.secret"
"$bin/cairn-sync" --root "$work/a" sync >/dev/null
"$bin/cairn-sync" --root "$work/b" sync >/dev/null
test ! -e "$work/b/x.secret"
test -e "$work/a/x.secret"

# A server's TLS certificate is checked.
unset SSL_CERT_FILE
if SSL_CERT_FILE=/dev/null "$bin/cairn-sync" --root "$work/a" sync >/dev/null 2>&1; then
    echo "an untrusted certificate was accepted" >&2
    exit 1
fi

kill "$server_pid"; wait "$server_pid" 2>/dev/null || true; server_pid=""
"$bin/cairn-server" fsck --data "$work/data" | grep -q clean

if ! user_names | cmp -s - "$work/user-names" \
    || { [ -f "$work/user-roots.json" ] && ! cmp -s "$user_dir/roots.json" "$work/user-roots.json"; }; then
    echo "smoke test: $user_dir, this user's own list of roots, changed while the test ran" >&2
    exit 1
fi
echo "smoke test passed"
