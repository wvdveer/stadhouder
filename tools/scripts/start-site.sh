#!/usr/bin/env bash
# Build (if needed) and start cgi-fileserver, serving stadhouder/site/public_html.
# Also starts cron-sim - the harness's stand-in for cron - which fires the
# staged service executable (stadhouder/bin/) every SIMULATED minute, the
# way a real cron line fires a deployed application's service program
# every real minute. Override bind address/port with the ADDR / PORT env
# vars.
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/../.."
FILESERVER_DIR="$REPO_ROOT/tools/cgi-fileserver"
CRON_SIM_DIR="$REPO_ROOT/tools/cron-sim"
SITE_ROOT="$REPO_ROOT/site"
SITE_DIR="$SITE_ROOT/public_html"

ADDR="${ADDR:-127.0.0.1}"
PORT="${PORT:-8080}"

mkdir -p "$SITE_DIR/cgi-bin/stadhouder"

# Mirrors the deployed layout: stadhouder/cfg/ (instance config),
# stadhouder/state/ (file-based runtime state - stadhouder has no database)
# and stadhouder/bin/ (the cron-initiated service executable) live outside
# the web root, siblings of public_html/ (anything under the web root is
# directly downloadable over plain HTTP). Everything locates them via
# DOCUMENT_ROOT's parent, same as production. Never wiped - a developer's
# local config and state survive restarts.
mkdir -p "$SITE_ROOT/stadhouder/cfg" "$SITE_ROOT/stadhouder/state" "$SITE_ROOT/stadhouder/bin"

(cd "$FILESERVER_DIR" && cargo build 2>&1 | tail -5)
(cd "$CRON_SIM_DIR" && cargo build 2>&1 | tail -5)

SERVICE_BIN=""
for candidate in "$SITE_ROOT/stadhouder/bin/test-app" "$SITE_ROOT/stadhouder/bin/test-app.exe"; do
    [ -f "$candidate" ] && SERVICE_BIN="$candidate"
done
if [ -z "$SERVICE_BIN" ]; then
    echo "start-site.sh: no service executable staged yet in stadhouder/bin/ - cron-sim will keep retrying until copy-to-site.sh stages one" >&2
    SERVICE_BIN="$SITE_ROOT/stadhouder/bin/test-app"
fi

"$CRON_SIM_DIR/target/debug/cron-sim" \
    --exe "$SERVICE_BIN" \
    --document-root "$SITE_DIR" \
    > "$SITE_ROOT/cron-sim.log" 2>&1 &

exec "$FILESERVER_DIR/target/debug/fileserver" --dir "$SITE_DIR" --addr "$ADDR" --port "$PORT"
