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

# Terminal colors (off when piped or NO_COLOR is set).
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    CYAN=$(printf '\033[36m')
    GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m')
    RESET=$(printf '\033[0m')
else
    BOLD='' DIM='' CYAN='' GREEN='' YELLOW='' RESET=''
fi

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
    # temp root and install the binaries into install_dir for a consistent layout.
    cargo_root="$(mktemp -d)"
    if ! cargo install --locked --root "$cargo_root" --git "https://github.com/${repo}" greplm-cli greplm-mcp; then
        rm -rf "$cargo_root"
        return 1
    fi

    mkdir -p "$install_dir"
    for name in greplm greplm-mcp; do
        install_binary "$cargo_root/bin/$name" "$name"
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
    # Install via a temp file + atomic rename (never overwrite in place).
    # Overwriting a binary in place reuses the same inode, which on macOS
    # poisons the kernel's cached code signature and makes every launch die
    # with "SIGKILL (Code Signature Invalid)". A rename gives a fresh inode.
    tmp_dest="$dest.tmp.$$"
    cp "$src" "$tmp_dest"
    chmod +x "$tmp_dest"
    mv -f "$tmp_dest" "$dest"
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

echo
echo "  ╭────────────────────────────────────────────────────────────╮"
echo "  │                                                            │"
echo "  │    ${BOLD}${CYAN}greplm${RESET} — installed successfully                 │"
echo "  │    ${DIM}Code search for the agent loop${RESET}                        │"
echo "  │                                                            │"
echo "  ╰────────────────────────────────────────────────────────────╯"
printf "  %sInstall dir:%s  %s\n\n" "$DIM" "$RESET" "$install_dir"
printf "  %sGet started in 3 steps%s\n\n" "$BOLD" "$RESET"
printf "  %s①%s  %sSet up a project%s\n" "$YELLOW" "$RESET" "$BOLD"
printf "     %s\$ cd <your-project> && greplm setup%s\n" "$GREEN" "$RESET"
printf "     %sindex + warm daemon%s\n\n" "$DIM" "$RESET"
printf "  %s②%s  %sConnect your AI editor%s\n" "$YELLOW" "$RESET" "$BOLD"
printf "     %s\$ greplm mcp config%s\n" "$GREEN" "$RESET"
printf "     %spaste JSON → .cursor/mcp.json · Claude · VS Code%s\n\n" "$DIM" "$RESET"
printf "  %s③%s  %sTeach your editor%s\n" "$YELLOW" "$RESET" "$BOLD"
printf "     %s\$ greplm agent add%s\n" "$GREEN" "$RESET"
printf "     %sauto-detects Cursor, Claude, Copilot, …%s\n\n" "$DIM" "$RESET"
echo "  ──────────────────────────────────────────────────────────────"
printf "  %sShow steps again:%s  greplm welcome\n" "$DIM" "$RESET"
printf "  %sBinaries:%s         ${install_dir}/greplm\n" "$DIM" "$RESET"
printf "                      ${install_dir}/greplm-mcp\n"
echo
