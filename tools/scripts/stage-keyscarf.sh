#!/usr/bin/env bash
# Unpacks the KeyScarf release archive fetched by fetch-keyscarf.sh and
# overlays it onto the local site/ tree - the exact layout a real cPanel
# install of KeyScarf would have next to a stadhouder deployment:
#   public_html/*  ->  site/public_html/   (landing pages, wasm apps, cgi-bin/keyscarf/)
#   keyscarf/*     ->  site/keyscarf/      (cfg/, db/, images/ - outside the web root)
# (The archive's docs/ folder is not staged.)
#
# Safe to re-run: it replaces KeyScarf's files and leaves everything else in
# site/ alone. First-time KeyScarf initialisation (DB connection details +
# first admin account) happens through its own setup wizard at
# /keyscarf/setup/setup.html once the site is running - exactly as a real
# admin would do it.
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/../.."
DOWNLOADS_DIR="$REPO_ROOT/tools/downloads"
SITE_ROOT="$REPO_ROOT/site"

# Same default/override dance as fetch-keyscarf.sh, so the two scripts
# always agree on which archive is in play.
KEYSCARF_VERSION="0_1_0"
if [ -f "$REPO_ROOT/dev_env/keyscarf.conf" ]; then
    # shellcheck source=/dev/null
    source "$REPO_ROOT/dev_env/keyscarf.conf"
fi

case "$(uname -s)" in
    Linux) ARCHIVE="keyscarf_linux_${KEYSCARF_VERSION}.tar.gz" ;;
    MINGW* | MSYS* | CYGWIN*) ARCHIVE="keyscarf_windows_${KEYSCARF_VERSION}.zip" ;;
    *) echo "stage-keyscarf.sh: unsupported host OS '$(uname -s)'" >&2; exit 1 ;;
esac

ARCHIVE_PATH="$DOWNLOADS_DIR/$ARCHIVE"
if [ ! -f "$ARCHIVE_PATH" ]; then
    echo "stage-keyscarf.sh: $ARCHIVE_PATH not found - run tools/scripts/fetch-keyscarf.sh first" >&2
    exit 1
fi

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

echo "==> Unpacking $ARCHIVE"
case "$ARCHIVE" in
    *.tar.gz)
        tar -xzf "$ARCHIVE_PATH" -C "$WORK_DIR"
        ;;
    *.zip)
        if command -v unzip >/dev/null 2>&1; then
            # unzip exits 1 (not 0) on a mere warning, e.g. the
            # backslash-path-separator notice it prints for zips built via
            # PowerShell's Compress-Archive - extraction itself still
            # succeeds, so only treat exit codes above 1 as a real failure.
            status=0
            unzip -qo "$ARCHIVE_PATH" -d "$WORK_DIR" || status=$?
            if [ "$status" -gt 1 ]; then
                echo "stage-keyscarf.sh: unzip failed (exit $status)" >&2
                exit 1
            fi
        else
            # Git Bash ships no unzip; fall back to Windows PowerShell.
            powershell.exe -NoProfile -Command \
                "Expand-Archive -Path '$(cygpath -w "$ARCHIVE_PATH")' -DestinationPath '$(cygpath -w "$WORK_DIR")' -Force" \
                || { echo "stage-keyscarf.sh: Expand-Archive failed" >&2; exit 1; }
        fi
        ;;
esac

for dir in public_html keyscarf; do
    if [ ! -d "$WORK_DIR/$dir" ]; then
        echo "stage-keyscarf.sh: archive has no $dir/ folder - not a KeyScarf release package?" >&2
        exit 1
    fi
done

echo "==> Staging into site/"
mkdir -p "$SITE_ROOT/public_html" "$SITE_ROOT/keyscarf/cfg"
cp -r "$WORK_DIR/public_html/." "$SITE_ROOT/public_html/"
cp -r "$WORK_DIR/keyscarf/." "$SITE_ROOT/keyscarf/"

# Pre-seed the DB connection, if a developer has provided one (see
# dev_env/keyscarf-db.conf.example) and it isn't already set up - lets the
# setup wizard skip straight to schema installation / first-admin creation.
# Never overwrites an existing authsite.conf: a working config must never
# be silently replaced (same rule KeyScarf's own wizard follows).
AUTHSITE_CONF="$SITE_ROOT/keyscarf/cfg/authsite.conf"
DB_CONF="$REPO_ROOT/dev_env/keyscarf-db.conf"
if [ -f "$DB_CONF" ] && [ ! -f "$AUTHSITE_CONF" ]; then
    echo "==> Pre-seeding $AUTHSITE_CONF from dev_env/keyscarf-db.conf"
    cp "$DB_CONF" "$AUTHSITE_CONF"
fi

echo "==> KeyScarf staged. If this is a fresh install, initialise it via its"
echo "    setup wizard: http://127.0.0.1:8080/keyscarf/setup/setup.html"
