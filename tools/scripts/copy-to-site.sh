#!/usr/bin/env bash
# Copy built stadhouder binaries into the local site, routing each to where
# it lives on a real deployment:
#   test-app(.exe)   -> site/stadhouder/bin/    (the application's service
#                                                program - here the harness's
#                                                test-app - outside the web
#                                                root, started by cron)
#   everything else  -> site/public_html/cgi-bin/stadhouder/  (CGI endpoints)
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SITE_CGI_BIN="$SCRIPT_DIR/../../site/public_html/cgi-bin/stadhouder"
SERVICE_BIN_DIR="$SCRIPT_DIR/../../site/stadhouder/bin"

if [ $# -lt 1 ]; then
    echo "Usage: copy-to-site.sh <binary> [<binary> ...]" >&2
    echo "Example: copy-to-site.sh rust/backend/target/debug/*.exe tools/test-app/target/debug/*.exe" >&2
    exit 1
fi

# Wipe the destinations first - they must be an exact mirror of what's
# passed in, never a superset. Cargo never deletes a compiled binary just
# because its [[bin]] entry was renamed/removed, so a stale binary from a
# prior build (e.g. a renamed-away endpoint) could otherwise keep being
# served under its old name indefinitely - a real "ghost endpoint" risk,
# not just clutter.
rm -rf "$SITE_CGI_BIN" "$SERVICE_BIN_DIR"
mkdir -p "$SITE_CGI_BIN" "$SERVICE_BIN_DIR"

for bin in "$@"; do
    if [ ! -f "$bin" ]; then
        echo "warning: '$bin' not found, skipping" >&2
        continue
    fi
    case "$(basename "$bin")" in
        test-app | test-app.exe)
            cp "$bin" "$SERVICE_BIN_DIR/"
            echo "Copied $(basename "$bin") -> $SERVICE_BIN_DIR/"
            ;;
        *)
            cp "$bin" "$SITE_CGI_BIN/"
            echo "Copied $(basename "$bin") -> $SITE_CGI_BIN/"
            ;;
    esac
done
