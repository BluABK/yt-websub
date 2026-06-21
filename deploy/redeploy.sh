#!/bin/sh
# redeploy.sh -- rebuild and redeploy yt-websub.
#
# Run from the directory that CONTAINS the yt-websub/ checkout, e.g.:
#     sh yt-websub/deploy/redeploy.sh
# (or copy this file next to the checkout and run ./redeploy.sh)
#
# Builds the release binary; ONLY if the build succeeds does it install the
# binary and restart the service. Returns to the starting directory either way,
# and exits non-zero if the build failed.

set -u

SRC_DIR="yt-websub"
BIN_PATH="/usr/local/bin/yt-websub"
SERVICE="yt-websub"

# Elevate the install/restart only when not already root. The build runs as the
# invoking user so it uses that user's rustup toolchain.
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

START_DIR="$(pwd)"

if [ ! -d "$SRC_DIR" ]; then
    echo "error: '$SRC_DIR/' not found in $(pwd)." >&2
    echo "       Run this from the folder that contains the yt-websub checkout." >&2
    exit 1
fi

cd "$SRC_DIR" || exit 1

echo "==> Building release (cargo build --release)..."
if cargo build --release; then
    echo "==> Build OK -- installing $BIN_PATH"
    $SUDO install -m 0755 target/release/yt-websub "$BIN_PATH"
    echo "==> Restarting $SERVICE"
    $SUDO systemctl restart "$SERVICE"
    if $SUDO systemctl is-active --quiet "$SERVICE"; then
        echo "==> $SERVICE is active."
    else
        echo "warning: $SERVICE is not active -- check: journalctl -u $SERVICE -n 30 --no-pager" >&2
    fi
    RC=0
else
    echo "error: build failed -- not installing or restarting." >&2
    RC=1
fi

cd "$START_DIR"
exit "$RC"
