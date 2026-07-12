#!/usr/bin/env bash
# Builds the dev-only tools under tools/ that other scripts depend on at
# runtime - not part of the deployed stadhouder product. Currently:
#   - cgi-fileserver: used by start-site.sh to serve/execute CGI binaries
#     locally, including in CI before running system-test.sh.
#   - cron-sim: the harness's stand-in for cron - started by start-site.sh,
#     fires the staged service executable every simulated minute.
#   - test-app: the harness's stand-in for a real application built on the
#     stadhouder library - cron-sim runs it as the site's service program
#     (staged into site/stadhouder/bin/ by copy-to-site.sh).
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/../.."

echo "==> Building tools/cgi-fileserver"
cd "$REPO_ROOT/tools/cgi-fileserver"
cargo build

echo "==> Building tools/cron-sim"
cd "$REPO_ROOT/tools/cron-sim"
cargo build

echo "==> Building tools/test-app"
cd "$REPO_ROOT/tools/test-app"
cargo build

echo "==> Tools build complete"
