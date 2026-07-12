#!/usr/bin/env bash
# Downloads the KeyScarf release archive for this host OS into
# tools/downloads/ (gitignored). KeyScarf is the identity provider the test
# harness runs against; stadhouder itself stays IdP-agnostic - the harness
# just needs *an* IdP, installed the same way a real admin would install one.
#
# The release location defaults to the official GitHub release. A developer
# can point it somewhere else (e.g. a locally built package) by creating
# dev_env/keyscarf.conf (gitignored; see dev_env/keyscarf.conf.example):
#   KEYSCARF_RELEASE_BASE=file:///C:/projects/rust/usermgr/package/
# Both http(s):// and file:// URLs work - curl handles either.
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/../.."
DOWNLOADS_DIR="$REPO_ROOT/tools/downloads"

# Defaults - override in dev_env/keyscarf.conf, which is sourced after.
KEYSCARF_VERSION="0_1_0"
KEYSCARF_RELEASE_BASE="https://github.com/wvdveer/keyscarf/releases/download/${KEYSCARF_VERSION}/"

if [ -f "$REPO_ROOT/dev_env/keyscarf.conf" ]; then
    # shellcheck source=/dev/null
    source "$REPO_ROOT/dev_env/keyscarf.conf"
fi

case "$(uname -s)" in
    Linux) ARCHIVE="keyscarf_linux_${KEYSCARF_VERSION}.tar.gz" ;;
    MINGW* | MSYS* | CYGWIN*) ARCHIVE="keyscarf_windows_${KEYSCARF_VERSION}.zip" ;;
    *) echo "fetch-keyscarf.sh: unsupported host OS '$(uname -s)'" >&2; exit 1 ;;
esac

mkdir -p "$DOWNLOADS_DIR"
URL="${KEYSCARF_RELEASE_BASE%/}/$ARCHIVE"

echo "==> Fetching $URL"
curl -fL "$URL" -o "$DOWNLOADS_DIR/$ARCHIVE"
echo "==> Saved tools/downloads/$ARCHIVE"
echo "    Next: tools/scripts/stage-keyscarf.sh to unpack it into site/"
