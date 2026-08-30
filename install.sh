#!/usr/bin/env bash
#
# cryptocli installer
#
#   curl -fsSL https://raw.githubusercontent.com/codingsushi79/ccli/main/install.sh | bash
#
# Environment overrides:
#   CCLI_INSTALL_DIR   where to put the binary (default: first sensible dir on PATH)
#   CCLI_REPO          owner/name to clone from     (default: codingsushi79/ccli)
#   CCLI_BRANCH        branch to build              (default: main)
#   CCLI_NO_COLOR=1    plain output
#
set -euo pipefail

REPO="${CCLI_REPO:-codingsushi79/ccli}"
BRANCH="${CCLI_BRANCH:-main}"
BIN_NAME="cryptocli"
ALIAS_NAME="ccli"

# ------------------------------------------------------------------ styling --

if [ -t 1 ] && [ -z "${CCLI_NO_COLOR:-}" ] && [ "${TERM:-dumb}" != "dumb" ]; then
    B=$'\033[1m'; DIM=$'\033[2m'; R=$'\033[0m'
    BLUE=$'\033[38;5;111m'; GREEN=$'\033[38;5;150m'
    AMBER=$'\033[38;5;179m'; RED=$'\033[38;5;204m'; PURPLE=$'\033[38;5;141m'
else
    B=""; DIM=""; R=""; BLUE=""; GREEN=""; AMBER=""; RED=""; PURPLE=""
fi

BOX_W=62
RULE="$(printf '─%.0s' $(seq 1 $BOX_W))"

# Pad to the box width using the *visible* length, so colour codes inside a
# line do not push the right border out of alignment.
box_line() {
    local edge="$1" rendered="$2" plain pad
    plain="$(printf '%s' "$rendered" | sed $'s/\033\[[0-9;]*m//g')"
    pad=$(( BOX_W - ${#plain} - 2 ))
    if [ "$pad" -lt 0 ]; then pad=0; fi
    printf '   %s│%s %s%*s %s│%s\n' "$edge" "$R" "$rendered" "$pad" "" "$edge" "$R"
}

box_top()    { printf '   %s┌%s┐%s\n' "$1" "$RULE" "$R"; }
box_bottom() { printf '   %s└%s┘%s\n' "$1" "$RULE" "$R"; }

banner() {
    printf '\n'
    box_top "$BLUE"
    box_line "$BLUE" ""
    box_line "$BLUE" "$B$PURPLE   __   __   _ _$R"
    box_line "$BLUE" "$B$PURPLE  / _| / _| | (_)$R      ${B}c r y p t o c l i${R}"
    box_line "$BLUE" "$B$PURPLE | (_ | (_| | | |$R      ${DIM}multi-coin mining, live TUI,${R}"
    box_line "$BLUE" "$B$PURPLE  \\__| \\__|_|_|_|$R      ${DIM}and a daemon that outlives it${R}"
    box_line "$BLUE" ""
    box_bottom "$BLUE"
    printf '\n'
}

step()  { printf '   %s▸%s %s\n' "$BLUE$B" "$R" "$1"; }
ok()    { printf '   %s✓%s %s\n' "$GREEN$B" "$R" "$1"; }
info()  { printf '     %s%s%s\n' "$DIM" "$1" "$R"; }
warn()  { printf '   %s!%s %s\n' "$AMBER$B" "$R" "$1"; }
die()   { printf '\n   %s✗ %s%s\n\n' "$RED$B" "$1" "$R" >&2; exit 1; }

# Run a command quietly with a spinner; dump its log only if it fails.
spin() {
    local message="$1"; shift
    local log; log="$(mktemp)"
    local frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
    "$@" >"$log" 2>&1 &
    local pid=$!
    local start; start=$SECONDS
    if [ -t 1 ]; then
        local i=0
        while kill -0 "$pid" 2>/dev/null; do
            i=$(( (i + 1) % 10 ))
            printf '\r   %s%s%s %s %s(%ss)%s ' \
                "$BLUE" "${frames:$i:1}" "$R" "$message" "$DIM" "$((SECONDS - start))" "$R"
            sleep 0.1
        done
        printf '\r\033[K'
    else
        printf '   … %s\n' "$message"
    fi
    if wait "$pid"; then
        ok "$message $DIM($((SECONDS - start))s)$R"
        rm -f "$log"
    else
        printf '\n'
        warn "$message failed:"
        tail -n 25 "$log" | sed 's/^/     /'
        rm -f "$log"
        exit 1
    fi
}

cleanup() {
    if [ -n "${TMP_DIR:-}" ]; then rm -rf "$TMP_DIR"; fi
    return 0
}
trap cleanup EXIT

# ------------------------------------------------------------ requirements --

banner

case "$(uname -s)" in
    Linux|Darwin) ;;
    *) die "cryptocli needs Linux or macOS (the daemon uses a Unix socket)." ;;
esac

step "Checking prerequisites"
if ! command -v cargo >/dev/null 2>&1; then
    printf '\n'
    warn "Rust is not installed."
    info "cryptocli builds from source. Install Rust, then re-run this script:"
    printf '\n     %scurl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh%s\n\n' "$B" "$R"
    exit 1
fi
ok "cargo $(cargo --version | awk '{print $2}')"

# Tell the user up front which hashing backend they will get, since it is worth
# roughly a 6x difference.
BACKEND="scalar"
if [ "$(uname -s)" = "Linux" ] && grep -qm1 ' avx2' /proc/cpuinfo 2>/dev/null; then
    BACKEND="AVX2 8-way"
elif [ "$(uname -s)" = "Darwin" ] && sysctl -n machdep.cpu.leaf7_features 2>/dev/null | grep -q AVX2; then
    BACKEND="AVX2 8-way"
fi
if [ "$BACKEND" = "scalar" ]; then
    ok "hashing backend: scalar"
    info "no AVX2 on this CPU — mining will work, just slower"
else
    ok "hashing backend: $BACKEND"
fi

# ------------------------------------------------------------------ source --

if [ -f "Cargo.toml" ] && grep -q '^name = "cryptocli"' Cargo.toml 2>/dev/null; then
    SRC_DIR="$PWD"
    step "Building from the current checkout"
else
    command -v git >/dev/null 2>&1 || die "git is required to fetch the source."
    TMP_DIR="$(mktemp -d)"
    SRC_DIR="$TMP_DIR/ccli"
    step "Fetching $REPO"
    spin "cloning $BRANCH" git clone --depth 1 --branch "$BRANCH" \
        "https://github.com/$REPO.git" "$SRC_DIR"
fi

step "Building (this takes a minute the first time)"
spin "cargo build --release" cargo build --release --manifest-path "$SRC_DIR/Cargo.toml"

BUILT="$SRC_DIR/target/release/$BIN_NAME"
[ -x "$BUILT" ] || die "build finished but $BUILT is missing."

# ----------------------------------------------------------------- install --

# Prefer a directory that is already on PATH, so the command works immediately.
pick_dir() {
    if [ -n "${CCLI_INSTALL_DIR:-}" ]; then
        printf '%s' "$CCLI_INSTALL_DIR"; return
    fi
    local candidates="$HOME/.cargo/bin $HOME/.local/bin /usr/local/bin"
    for dir in $candidates; do
        case ":$PATH:" in *":$dir:"*)
            if [ -d "$dir" ] && [ -w "$dir" ]; then printf '%s' "$dir"; return; fi
        esac
    done
    for dir in $candidates; do
        if [ -d "$dir" ] && [ -w "$dir" ]; then printf '%s' "$dir"; return; fi
    done
    printf '%s' "$HOME/.local/bin"
}

INSTALL_DIR="$(pick_dir)"
mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] || die "$INSTALL_DIR is not writable. Set CCLI_INSTALL_DIR to somewhere you own."

step "Installing"
install -m 755 "$BUILT" "$INSTALL_DIR/$BIN_NAME"
ln -sf "$INSTALL_DIR/$BIN_NAME" "$INSTALL_DIR/$ALIAS_NAME"
ok "$INSTALL_DIR/$BIN_NAME"
info "also linked as \`$ALIAS_NAME\`"

VERSION="$("$INSTALL_DIR/$BIN_NAME" --version 2>/dev/null || echo "$BIN_NAME")"
ok "$VERSION"

# ---------------------------------------------------------------- PATH help --

ON_PATH=0
case ":$PATH:" in *":$INSTALL_DIR:"*) ON_PATH=1 ;; esac

if [ "$ON_PATH" -eq 0 ]; then
    # Work out which file actually gets read by an interactive shell, because
    # ~/.profile is only sourced by *login* shells and that trips people up.
    case "$(basename "${SHELL:-bash}")" in
        zsh)  RC="$HOME/.zshrc" ;;
        fish) RC="$HOME/.config/fish/config.fish" ;;
        *)    RC="$HOME/.bashrc" ;;
    esac
    printf '\n'
    warn "$INSTALL_DIR is not on your PATH."
    if [ "$(basename "${SHELL:-bash}")" = "fish" ]; then
        info "Add it with:"
        printf '\n     %sfish_add_path %s%s\n' "$B" "$INSTALL_DIR" "$R"
    else
        info "Add it to $RC (interactive shells do not read ~/.profile):"
        printf '\n     %secho '"'"'export PATH="%s:$PATH"'"'"' >> %s%s\n' \
            "$B" "$INSTALL_DIR" "$RC" "$R"
    fi
    printf '     %sthen: source %s%s\n' "$B" "$RC" "$R"
fi

# ------------------------------------------------------------- next steps ---

printf '\n'
box_top "$GREEN"
box_line "$GREEN" "${B}Installed.${R} Two ways to set up:"
box_line "$GREEN" ""
box_line "$GREEN" "${DIM}1. In the dashboard  ${R}${B}cryptocli${R}${DIM}, then press ${R}${B}a${R}${DIM} to add${R}"
box_line "$GREEN" "${DIM}   a wallet, a rig or an endpoint. ${R}${B}c${R}${DIM} adds a second${R}"
box_line "$GREEN" "${DIM}   coin to a rig, mined at the same time.${R}"
box_line "$GREEN" ""
box_line "$GREEN" "${DIM}2. From the shell${R}"
box_line "$GREEN" "   ${B}cryptocli wallet add main --coin BTC --address bc1..${R}"
box_line "$GREEN" "   ${B}cryptocli rig add btc --url stratum+tcp://pool:3333${R}"
box_line "$GREEN" "       ${B}--wallet main --coin BTC${R}"
box_line "$GREEN" "   ${B}cryptocli${R}"
box_line "$GREEN" ""
box_bottom "$GREEN"
printf '\n'
printf '   %sQuitting the dashboard does not stop mining — the daemon keeps%s\n' "$DIM" "$R"
printf '   %sgoing. Reopen with %scryptocli%s%s any time, or stop with %sQ%s.%s\n\n' \
    "$DIM" "$R$B" "$R" "$DIM" "$B" "$R$DIM" "$R"
