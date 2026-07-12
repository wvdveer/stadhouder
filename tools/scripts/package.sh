#!/usr/bin/env bash
# Builds release distribution(s) of stadhouder and archives them under
# package/. Unlike KeyScarf (which ships ready-to-run binaries a consumer
# never compiles), an application built on stadhouder must compile the
# `common` and `service` crates INTO ITS OWN service program - Rust has no
# stable ABI for shipping those pre-built. So the package ships their
# SOURCE (identical for every target, built once and reused - same
# reasoning as KeyScarf's wasm frontends) alongside a PRE-BUILT `client`
# CGI binary per target, which a consuming application stages as-is.
#
# Usage:
#   package.sh                 # build for the OS this runs on
#   package.sh --target-os OS  # build for OS (windows | linux), cross if needed
#   package.sh --all           # build for both windows and linux
#   ... --windows-msvc         # build the Windows target against MSVC instead
#                              #   of the default GNU/mingw (Windows host only)
#
# Output per target:
#   package/<os>/                       - the marshalling folder
#     src/common/, src/service/         - shared crate source (identical
#                                         across targets, from the host's
#                                         first build)
#     bin/client(.exe)                  - the pre-built client CGI binary
#   package/stadhouder_<os>_<ver>.<ext> - archive of that folder's contents
#     windows -> stadhouder_windows_0_1_0.zip
#     linux   -> stadhouder_linux_0_1_0.tar.gz
#
# The client CGI binary is a RELEASE build.
#   Linux target   -> musl static (native on Linux; cross from Windows via
#                     cargo-zigbuild).
#   Windows target -> GNU/mingw by default, on either host - a fully open
#                     toolchain (no Microsoft components or licence),
#                     self-contained via crt-static. Pass --windows-msvc to
#                     build against MSVC instead (Windows host only).
# All required tools are checked up front - the script fails loudly, listing
# everything missing, before building. (Adapted from KeyScarf's own
# package.sh - see that project for the fuller original.)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BACKEND="$REPO_ROOT/rust/backend"

die() { echo "ERROR: $*" >&2; exit 1; }

case "$(uname -s)" in
    Linux) HOST_OS=linux ;;
    MINGW* | MSYS* | CYGWIN*) HOST_OS=windows ;;
    *) die "unsupported host OS '$(uname -s)'" ;;
esac

# --- Parse args -------------------------------------------------------------
WINDOWS_TOOLCHAIN=gnu
MINGW_CC=""
TARGETS=()
targets_set=""
while [ $# -gt 0 ]; do
    case "$1" in
        --all) TARGETS=(linux windows); targets_set=1; shift ;;
        --target-os)
            case "${2:-}" in
                windows | linux) TARGETS=("$2"); targets_set=1; shift 2 ;;
                *) die "--target-os needs 'windows' or 'linux'" ;;
            esac
            ;;
        --windows-msvc) WINDOWS_TOOLCHAIN=msvc; shift ;;
        *) die "unknown argument '$1' (use --all, --target-os <os>, --windows-msvc, or no argument)" ;;
    esac
done
[ -n "$targets_set" ] || TARGETS=("$HOST_OS")

# --- Per-target build/output details ----------------------------------------
win_triple() { [ "$WINDOWS_TOOLCHAIN" = msvc ] && echo x86_64-pc-windows-msvc || echo x86_64-pc-windows-gnu; }
bin_dir_for() {
    case "$1" in
        linux) echo "$BACKEND/target/x86_64-unknown-linux-musl/release" ;;
        windows) echo "$BACKEND/target/$(win_triple)/release" ;;
    esac
}
suffix_for() { [ "$1" = windows ] && echo ".exe" || echo ""; }
ext_for() { [ "$1" = windows ] && echo "zip" || echo "tar.gz"; }

# --- Fail loudly up front if any required tool is missing -------------------
MISSING=()
have() { command -v "$1" >/dev/null 2>&1; }
need_cmd() { have "$1" || MISSING+=("$2"); }

need_rust_target() {
    local sysroot
    sysroot=$(rustc --print sysroot 2>/dev/null)
    if [ -z "$sysroot" ] || [ ! -d "$sysroot/lib/rustlib/$1/lib" ]; then
        MISSING+=("rust std for target $1  (rustup target add $1, or your distro's rust-std package for it)")
    fi
}

need_cmd cargo "cargo  (install Rust: https://rustup.rs)"

for t in "${TARGETS[@]}"; do
    if [ "$t" = linux ]; then
        need_rust_target x86_64-unknown-linux-musl
        if [ "$HOST_OS" != linux ]; then
            need_cmd cargo-zigbuild "cargo-zigbuild  (cargo install cargo-zigbuild)"
            need_cmd zig "zig  (https://ziglang.org/download - put it on PATH)"
        fi
    elif [ "$WINDOWS_TOOLCHAIN" = msvc ]; then
        [ "$HOST_OS" = windows ] || die "--windows-msvc needs a Windows host; on Linux only the default GNU Windows build is available. Drop --windows-msvc."
        need_rust_target x86_64-pc-windows-msvc
    else
        need_rust_target x86_64-pc-windows-gnu
        if have x86_64-w64-mingw32-gcc; then
            MINGW_CC=x86_64-w64-mingw32-gcc
        elif have gcc && gcc -dumpmachine 2>/dev/null | grep -q '^x86_64.*mingw'; then
            MINGW_CC=gcc
        else
            MISSING+=("an x86_64 mingw-w64 gcc  (Debian/Ubuntu: sudo apt install gcc-mingw-w64-x86-64; Windows: an x86_64 WinLibs/MSYS2 toolchain on PATH)")
        fi
    fi

    if [ "$(ext_for "$t")" = zip ]; then
        if [ "$HOST_OS" = windows ]; then
            need_cmd powershell.exe "powershell.exe  (built into Windows)"
            need_cmd cygpath "cygpath  (ships with Git for Windows)"
        else
            need_cmd zip "zip  (Debian/Ubuntu: sudo apt install zip)"
        fi
    else
        need_cmd tar "tar"
    fi
done

if [ ${#MISSING[@]} -gt 0 ]; then
    {
        echo "Cannot build the requested package(s) - missing tools:"
        printf '  - %s\n' "${MISSING[@]}"
    } >&2
    exit 1
fi

# --- Version (0.1.0 -> 0_1_0) -----------------------------------------------
VERSION=$(grep -m1 '^version = ' "$BACKEND/service/Cargo.toml" | sed -E 's/^version = "([^"]+)".*/\1/')
[ -n "$VERSION" ] || die "couldn't read the version from service/Cargo.toml"
VERSION_US=${VERSION//./_}

echo "==> Packaging stadhouder $VERSION for: ${TARGETS[*]}  (host: $HOST_OS)"

# --- Build the client CGI binary, per target ---------------------------------
build_backend_for() {
    local t="$1"
    echo "==> Building client CGI binary for $t (release)"
    if [ "$t" = linux ]; then
        if [ "$HOST_OS" = linux ]; then
            (cd "$BACKEND" && cargo build --release --target x86_64-unknown-linux-musl -p api --bin client)
        else
            (cd "$BACKEND" && cargo zigbuild --release --target x86_64-unknown-linux-musl -p api --bin client)
        fi
    elif [ "$WINDOWS_TOOLCHAIN" = msvc ]; then
        (cd "$BACKEND" && cargo build --release --target x86_64-pc-windows-msvc -p api --bin client)
    else
        local mingw_bin
        mingw_bin="$(dirname "$(command -v "$MINGW_CC")")"
        (cd "$BACKEND" \
            && PATH="$mingw_bin:$PATH" \
               CC_x86_64_pc_windows_gnu="$MINGW_CC" \
               CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$MINGW_CC" \
               RUSTFLAGS="-C target-feature=+crt-static" \
               cargo build --release --target x86_64-pc-windows-gnu -p api --bin client)
    fi
}

# --- Marshal + archive, per target -------------------------------------------
marshal_and_archive() {
    local t="$1"
    local pkg_dir="$REPO_ROOT/package/$t"
    local archive="$REPO_ROOT/package/stadhouder_${t}_${VERSION_US}.$(ext_for "$t")"
    local bin_dir suffix
    bin_dir="$(bin_dir_for "$t")"
    suffix="$(suffix_for "$t")"

    echo "==> Marshalling into $pkg_dir"
    rm -rf "$pkg_dir"
    mkdir -p "$pkg_dir/src/common/src" "$pkg_dir/src/service/src" "$pkg_dir/bin"

    # Crate source - identical across targets, so this is harmless to
    # redo per target (cheap, and keeps marshal_and_archive self-contained).
    cp "$BACKEND/common/Cargo.toml" "$pkg_dir/src/common/"
    cp -r "$BACKEND/common/src/." "$pkg_dir/src/common/src/"
    cp "$BACKEND/service/Cargo.toml" "$pkg_dir/src/service/"
    cp -r "$BACKEND/service/src/." "$pkg_dir/src/service/src/"

    local client_src="$bin_dir/client$suffix"
    [ -f "$client_src" ] || die "missing release binary '$client_src' - did the build for $t succeed?"
    cp "$client_src" "$pkg_dir/bin/"

    echo "==> Creating $archive"
    rm -f "$archive"
    if [ "$(ext_for "$t")" = tar.gz ]; then
        tar -czf "$archive" -C "$pkg_dir" .
    elif [ "$HOST_OS" = windows ]; then
        powershell.exe -NoProfile -Command \
            "Compress-Archive -Path '$(cygpath -w "$pkg_dir")\\*' -DestinationPath '$(cygpath -w "$archive")' -Force" \
            || die "Compress-Archive failed"
    else
        (cd "$pkg_dir" && zip -qr "$archive" .)
    fi
    echo "==> Done: $archive"
}

for t in "${TARGETS[@]}"; do
    build_backend_for "$t"
    marshal_and_archive "$t"
done
