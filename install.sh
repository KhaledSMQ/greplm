#!/bin/sh
# Install greplm and greplm-mcp.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.sh | sh
#
# Environment:
#   GREPLM_VERSION   Release tag (e.g. v0.1.0). Default: latest GitHub release.
#   GREPLM_INSTALL   Install directory. Default: $CARGO_HOME/bin or ~/.local/bin
#   GREPLM_REPO      GitHub repo slug. Default: KhaledSMQ/greplm

set -eu

# Propagate failures through pipes (curl | tar).
(set -o pipefail 2>/dev/null) && set -o pipefail

do_curl() {
    curl --retry 5 -L --proto '=https' --tlsv1.2 -sSf "$@"
}

repo="${GREPLM_REPO:-KhaledSMQ/greplm}"
version="${GREPLM_VERSION:-}"

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
if [ -n "${GREPLM_INSTALL:-}" ]; then
    install_dir="$GREPLM_INSTALL"
elif [ -d "$cargo_home/bin" ] || command -v cargo >/dev/null 2>&1; then
    install_dir="$cargo_home/bin"
else
    install_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
fi

install_from_cargo() {
    if ! command -v cargo >/dev/null 2>&1; then
        return 1
    fi
    echo "Building from source with cargo (this may take a few minutes)..."
    # cargo install --root <dir> always writes to <dir>/bin, so build into a
    # temp root and copy the binaries into install_dir for a consistent layout.
    cargo_root="$(mktemp -d)"
    if ! cargo install --locked --root "$cargo_root" --git "https://github.com/${repo}" greplm-cli greplm-mcp; then
        rm -rf "$cargo_root"
        return 1
    fi

    mkdir -p "$install_dir"
    for name in greplm greplm-mcp; do
        cp "$cargo_root/bin/$name" "$install_dir/$name"
        chmod +x "$install_dir/$name"
        echo "  installed $install_dir/$name"
    done
    rm -rf "$cargo_root"
}

detect_target() {
    os="$(uname -s)"
    machine="$(uname -m)"

    case "$os" in
        Darwin)
            case "$machine" in
                arm64 | aarch64) echo "aarch64-apple-darwin" ;;
                x86_64) echo "x86_64-apple-darwin" ;;
                *) return 1 ;;
            esac
            ;;
        Linux)
            case "$machine" in
                x86_64) echo "x86_64-unknown-linux-gnu" ;;
                aarch64 | arm64) echo "aarch64-unknown-linux-gnu" ;;
                armv7l) echo "armv7-unknown-linux-gnueabihf" ;;
                *) return 1 ;;
            esac
            ;;
        MINGW* | MSYS* | CYGWIN*)
            case "$machine" in
                x86_64) echo "x86_64-pc-windows-msvc" ;;
                *) return 1 ;;
            esac
            ;;
        *)
            return 1
            ;;
    esac
}

install_binary() {
    src="$1"
    name="$2"
    dest="$install_dir/$name"

    if [ ! -f "$src" ]; then
        echo "error: missing $src in release archive" >&2
        return 1
    fi

    mkdir -p "$install_dir"
    cp "$src" "$dest"
    chmod +x "$dest"
    echo "  installed $dest"
}

install_from_release() {
    target="$(detect_target)" || return 1

    if [ -z "$version" ]; then
        base="https://github.com/${repo}/releases/latest/download/greplm-${target}"
    else
        case "$version" in
            v*) tag="$version" ;;
            *) tag="v$version" ;;
        esac
        base="https://github.com/${repo}/releases/download/${tag}/greplm-${target}"
    fi

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT INT TERM
    cd "$tmp"

    case "$target" in
        *-pc-windows-msvc)
            archive="greplm-${target}.zip"
            if ! do_curl -o "$archive" "${base}.zip"; then
                return 1
            fi
            if command -v unzip >/dev/null 2>&1; then
                unzip -q "$archive"
            else
                echo "error: unzip is required on Windows" >&2
                return 1
            fi
            install_binary "greplm.exe" "greplm.exe" &&
                install_binary "greplm-mcp.exe" "greplm-mcp.exe"
            ;;
        *)
            archive="greplm-${target}.tar.gz"
            if ! do_curl "${base}.tar.gz" | tar -xzf -; then
                return 1
            fi
            install_binary "greplm" "greplm" &&
                install_binary "greplm-mcp" "greplm-mcp"
            ;;
    esac

    return 0
}

echo "Installing greplm into ${install_dir}..."

if install_from_release; then
    :
elif install_from_cargo; then
    :
else
    echo "error: could not install greplm." >&2
    echo "  - No prebuilt release for this platform yet (publish a tag to enable), or" >&2
    echo "  - cargo build from git failed." >&2
    echo "Install Rust from https://rustup.rs then run:" >&2
    echo "  cargo install --locked --git https://github.com/${repo} greplm-cli greplm-mcp" >&2
    exit 1
fi

case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
        echo
        echo "Add ${install_dir} to your PATH if greplm is not found:"
        echo "  export PATH=\"${install_dir}:\$PATH\""
        echo
        ;;
esac

echo "Done."
echo
echo "Next steps:"
echo "  cd <your project> && greplm setup   # build the index + start an always-on daemon"
echo "  greplm doctor                       # check everything is healthy"
echo "  greplm --help"
