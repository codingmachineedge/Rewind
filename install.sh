#!/usr/bin/env bash
#
# Rewind installer
#
#   curl -fsSL https://raw.githubusercontent.com/codingmachineedge/Rewind/master/install.sh | bash
#
# Downloads the latest Rewind release, installs the `rewind` binary, and helps
# you sort out runtime dependencies and PATH.
#
set -euo pipefail

PREFIX="[rewind]"
ASSET="rewind-x86_64-linux-gnu.tar.gz"
URL="https://github.com/codingmachineedge/Rewind/releases/latest/download/${ASSET}"

# Ubuntu 24.04 package names for the runtime shared libraries Rewind links against.
RUNTIME_DEPS="libgtk-4-1 libadwaita-1-0 gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-pipewire pipewire pulseaudio-utils xclip"

info()  { printf '%s %s\n' "$PREFIX" "$*"; }
warn()  { printf '%s WARNING: %s\n' "$PREFIX" "$*" >&2; }
err()   { printf '%s ERROR: %s\n' "$PREFIX" "$*" >&2; }
die()   { err "$*"; exit 1; }

# ---------------------------------------------------------------------------
# 1. Platform check
# ---------------------------------------------------------------------------
OS="$(uname -s 2>/dev/null || echo unknown)"
ARCH="$(uname -m 2>/dev/null || echo unknown)"

if [ "$OS" != "Linux" ]; then
    die "Rewind only supports Linux (detected: ${OS}). Sorry!"
fi
if [ "$ARCH" != "x86_64" ]; then
    die "Rewind only ships an x86_64 build (detected: ${ARCH}). Build from source instead: cargo build --release --features linux"
fi

# ---------------------------------------------------------------------------
# 2. Pick an install directory
# ---------------------------------------------------------------------------
# Precedence: explicit REWIND_INSTALL_DIR > root => /usr/local/bin > ~/.local/bin
if [ -n "${REWIND_INSTALL_DIR:-}" ]; then
    INSTALL_DIR="$REWIND_INSTALL_DIR"
elif [ "$(id -u)" = "0" ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="${HOME}/.local/bin"
fi

info "Install directory: ${INSTALL_DIR}"

# ---------------------------------------------------------------------------
# 3. Download the latest release tarball
# ---------------------------------------------------------------------------
TMPDIR="$(mktemp -d "${TMPDIR:-/tmp}/rewind-install.XXXXXX")"
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

TARBALL="${TMPDIR}/${ASSET}"

download() {
    # download <url> <dest>
    if command -v curl >/dev/null 2>&1; then
        curl -fSL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$2" "$1"
    else
        die "Neither curl nor wget is available. Please install one and re-run."
    fi
}

info "Downloading latest release..."
info "  ${URL}"
download "$URL" "$TARBALL" || die "Download failed."

# ---------------------------------------------------------------------------
# 4. Extract and install
# ---------------------------------------------------------------------------
info "Extracting..."
tar -xzf "$TARBALL" -C "$TMPDIR" || die "Failed to extract ${ASSET}."

if [ ! -f "${TMPDIR}/rewind" ]; then
    die "Archive did not contain a 'rewind' binary."
fi

mkdir -p "$INSTALL_DIR" || die "Could not create ${INSTALL_DIR}."

DEST="${INSTALL_DIR}/rewind"
info "Installing to ${DEST}"
install -m 0755 "${TMPDIR}/rewind" "$DEST" 2>/dev/null || {
    cp "${TMPDIR}/rewind" "$DEST" || die "Could not copy binary to ${INSTALL_DIR} (permission denied?)."
    chmod +x "$DEST"
}

if [ ! -x "$DEST" ]; then
    die "Installation verification failed: ${DEST} is missing or not executable."
fi
info "Installed rewind binary."

# ---------------------------------------------------------------------------
# 5. Runtime dependencies
# ---------------------------------------------------------------------------
apt_install_cmd() {
    printf 'sudo apt-get update && sudo apt-get install -y %s\n' "$RUNTIME_DEPS"
}

sudo_is_passwordless() {
    command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1
}

if command -v apt-get >/dev/null 2>&1; then
    if [ "${REWIND_INSTALL_DEPS:-0}" = "1" ] || [ "$(id -u)" = "0" ] || sudo_is_passwordless; then
        info "Installing runtime dependencies via apt-get..."
        if [ "$(id -u)" = "0" ]; then
            apt-get update && apt-get install -y $RUNTIME_DEPS || warn "apt-get failed; install these manually: $RUNTIME_DEPS"
        else
            sudo apt-get update && sudo apt-get install -y $RUNTIME_DEPS || warn "apt-get failed; install these manually: $RUNTIME_DEPS"
        fi
    else
        info "Rewind needs these runtime packages. Install them with:"
        printf '\n    %s\n\n' "$(apt_install_cmd)"
        info "(Set REWIND_INSTALL_DEPS=1 to let this installer do it automatically.)"
    fi
else
    info "Non-apt system detected. Make sure the equivalents of these are installed:"
    printf '\n    %s\n\n' "$RUNTIME_DEPS"
fi

# ---------------------------------------------------------------------------
# 6. PATH check
# ---------------------------------------------------------------------------
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*)
        : # already on PATH
        ;;
    *)
        warn "${INSTALL_DIR} is not on your PATH."
        info "Add it by appending this to your ~/.bashrc or ~/.profile:"
        printf '\n    export PATH="%s:$PATH"\n\n' "$INSTALL_DIR"
        ;;
esac

# ---------------------------------------------------------------------------
# 7. Shared library sanity check (warn only)
# ---------------------------------------------------------------------------
if command -v ldd >/dev/null 2>&1; then
    MISSING="$(ldd "$DEST" 2>/dev/null | awk '/not found/ {print "    " $1}' || true)"
    if [ -n "$MISSING" ]; then
        warn "Some shared libraries could not be resolved:"
        printf '%s\n' "$MISSING" >&2
        warn "Install the runtime dependencies above to fix these."
    fi
fi

# ---------------------------------------------------------------------------
# 8. Done
# ---------------------------------------------------------------------------
cat <<EOF

$PREFIX Success! Rewind is installed.

  * Launch it by running:  rewind
  * Recorded clips are saved to  ./clips  in the directory you run it from.

$PREFIX Note: this binary was built on Ubuntu 24.04 and needs a reasonably recent
$PREFIX glibc (>= 2.39). On older or non-Debian distros you may need to build from
$PREFIX source instead:

    cargo build --release --features linux

Enjoy your Rewind. :)
EOF
