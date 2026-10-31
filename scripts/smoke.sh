#!/usr/bin/env bash
# The release binaries, end to end, through a real TLS proxy: `smoke.sh <directory with the binaries>`.
set -euo pipefail
bin=$(cd "$1" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
proxy_pid="" server_pid=""
cleanup() {
    [ -n "$proxy_pid" ] && kill "$proxy_pid" 2>/dev/null || true
    [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
    chmod -R u+rwx "$work" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

"$bin/cairn-server" init --data "$work/data" >/dev/null
"$bin/cairn-server" serve --data "$work/data" --listen 127.0.0.1:0 >"$work/server.out" &
server_pid=$!
for _ in $(seq 100); do grep -q listening "$work/server.out" && break; sleep 0.05; done
backend=$(sed -n 's/^listening on //p' "$work/server.out")

# A private CA and a certificate from it, as a self-hosted proxy would have.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=smoke test CA" \
    -keyout "$work/ca-key.pem" -out "$work/ca.pem" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout "$work/key.pem" -out "$work/cert.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n' >"$work/ext"
openssl x509 -req -in "$work/cert.csr" -CA "$work/ca.pem" -CAkey "$work/ca-key.pem" -CAcreateserial -days 1 \
    -extfile "$work/ext" -out "$work/cert.pem" 2>/dev/null
python3 -I "$here/tls_proxy.py" "$work/cert.pem" "$work/key.pem" "$backend" >"$work/proxy.out" &
proxy_pid=$!
for _ in $(seq 100); do [ -s "$work/proxy.out" ] && break; sleep 0.05; done
url="https://localhost:$(cat "$work/proxy.out")"

export SSL_CERT_FILE="$work/ca.pem"
mkdir "$work/a" "$work/b"
"$bin/cairn" init --server "$url" --token-file "$work/data/token" --device-name a "$work/a" >/dev/null
"$bin/cairn" init --server "$url" --token-file "$work/data/token" --device-name b "$work/b" >/dev/null

# Metadata and names across two devices.
printf 'hello\n' >"$work/a/hello.txt"
printf '#!/bin/sh\n' >"$work/a/script.sh"; chmod 755 "$work/a/script.sh"
ln -s hello.txt "$work/a/link"
ln "$work/a/hello.txt" "$work/a/hard"
truncate -s 1T "$work/a/sparse"
mkdir "$work/a/node_modules"; echo kept >"$work/a/node_modules/x"
"$bin/cairn" --root "$work/a" sync >/dev/null
"$bin/cairn" --root "$work/b" sync >/dev/null
test "$(cat "$work/b/hello.txt")" = hello
test "$(stat -c %a "$work/b/script.sh")" = 755
test "$(readlink "$work/b/link")" = hello.txt
test "$(stat -c %i "$work/b/hello.txt")" = "$(stat -c %i "$work/b/hard")"
test "$(stat -c %s "$work/b/sparse")" = 1099511627776
test "$(stat -c %b "$work/b/sparse")" -lt 16
test "$(cat "$work/b/node_modules/x")" = kept

# A conflict keeps both, and a restore brings an old version back.
echo from-a >"$work/a/hello.txt"
"$bin/cairn" --root "$work/a" sync >/dev/null
echo from-b >"$work/b/hello.txt"
"$bin/cairn" --root "$work/b" sync >/dev/null
test "$(cat "$work/b/hello.txt")" = from-a
test "$(cat "$work/b/"hello\ \(conflict\ b\ *\).txt)" = from-b
"$bin/cairn" --root "$work/b" conflicts | grep -q copy
"$bin/cairn" --root "$work/a" sync >/dev/null
"$bin/cairn" --root "$work/a" restore hello.txt --at 1 >/dev/null
test "$(cat "$work/a/hello.txt")" = hello

# A folder with no marker pauses.
mkdir "$work/gone"
status=0; "$bin/cairn" --root "$work/gone" sync >"$work/paused.out" || status=$?
test "$status" = 2
grep -q "Sync is paused" "$work/paused.out"

# A server's TLS certificate is checked.
unset SSL_CERT_FILE
if SSL_CERT_FILE=/dev/null "$bin/cairn" --root "$work/a" sync >/dev/null 2>&1; then
    echo "an untrusted certificate was accepted" >&2
    exit 1
fi

kill "$server_pid"; wait "$server_pid" 2>/dev/null || true; server_pid=""
"$bin/cairn-server" fsck --data "$work/data" | grep -q clean
echo "smoke test passed"
