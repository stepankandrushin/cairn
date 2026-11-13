#!/usr/bin/env bash
# What CI's release build needs from the system: musl-tools, as it links C statically, and qemu-user, which runs
# the aarch64 client for its checks and smoke test (scripts/release.sh). GitHub's runners once could not resolve any of
# Ubuntu's own hosts for hours while everything else resolved, so apt only chooses the packages here, from the
# image's signed index; each file comes from the first of several mirrors of Ubuntu's archive that serves it,
# and dpkg installs it only once it matches the SHA256 that index gives it.
set -euo pipefail

mirrors=(
    http://azure.archive.ubuntu.com/ubuntu
    https://mirrors.edge.kernel.org/ubuntu
    https://mirror.math.princeton.edu/pub/ubuntu
    https://mirrors.ocf.berkeley.edu/ubuntu
)
packages=(musl-tools qemu-user)
sudo=; [ "$(id -u)" = 0 ] || sudo=sudo

dir=$(mktemp -d)
trap 'rm -rf -- "$dir"' EXIT
# One line a file still to install: 'URI' name size hash, the URI by the image's source.
apt-get install --no-install-recommends --print-uris -qq -y "${packages[@]}" > "$dir/uris"
if [ ! -s "$dir/uris" ]; then
    echo "${packages[*]}: installed already"
    exit 0
fi

declare -A dead
while read -r uri file _; do
    uri=${uri//\'/}
    case $uri in */pool/*) ;; *) echo "not a path of an archive: $uri" >&2; exit 1 ;; esac
    path=pool/${uri#*/pool/}
    # The index's record of that file, whichever architecture or variant of the package it is, by its name
    # there, which the URI has percent-encoded (a version's + and epoch's :).
    name=$(printf '%b' "${path//%/\\x}")
    sha=$(apt-cache show "${file%%_*}" | awk -v f="$name" 'BEGIN { RS = "" }
        index($0 "\n", "\nFilename: " f "\n") { if (match($0, /\nSHA256: [0-9a-f]+/)) print substr($0, RSTART + 9, RLENGTH - 9); exit }')
    if [ ${#sha} != 64 ]; then
        echo "apt's index gives no SHA256 for $path" >&2
        exit 1
    fi
    got=
    for m in "${mirrors[@]}"; do
        [ -z "${dead[$m]-}" ] || continue
        rc=0
        curl -fsS --retry 2 --connect-timeout 15 --max-time 300 -o "$dir/$file" "$m/$path" || rc=$?
        if [ $rc = 0 ]; then
            if [ "$(sha256sum < "$dir/$file" | cut -c1-64)" = "$sha" ]; then
                echo "$file from $m"
                got=1
                break
            fi
            echo "$file from $m does not match apt's index" >&2
        elif [ $rc = 6 ] || [ $rc = 7 ] || [ $rc = 28 ]; then
            # Not resolved, refused or timed out: the next files do not ask it again.
            dead[$m]=1
        fi
        rm -f -- "$dir/$file"
    done
    if [ -z "$got" ]; then
        echo "no mirror served $path" >&2
        exit 1
    fi
done < "$dir/uris"

$sudo dpkg -i "$dir"/*.deb
command -v musl-gcc qemu-aarch64
