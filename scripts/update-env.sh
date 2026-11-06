# Sourced by the release scripts. A release's programs update themselves from the feed of whoever
# publishes it, and install only what one of the keys built into them signed (crates/engine/src/update.rs):
# give both, CAIRN_UPDATE_FEED (the URL of update.json, the latest release's) and CAIRN_UPDATE_KEYS (base64
# Ed25519 public keys, comma-separated), or CAIRN_UPDATE=off for a build that is never updated. Without one
# or the other a release would quietly be one whose every copy has to be replaced by hand.
if [[ ${CAIRN_UPDATE-} == off ]]; then
    unset CAIRN_UPDATE_FEED CAIRN_UPDATE_KEYS
    echo "CAIRN_UPDATE=off: these programs are never updated" >&2
elif [[ -z ${CAIRN_UPDATE_FEED-} || -z ${CAIRN_UPDATE_KEYS-} ]]; then
    echo "set CAIRN_UPDATE_FEED and CAIRN_UPDATE_KEYS, the feed and keys this release's programs update from, or CAIRN_UPDATE=off" >&2
    exit 1
else
    export CAIRN_UPDATE_FEED CAIRN_UPDATE_KEYS
fi
