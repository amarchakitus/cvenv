#!/usr/bin/env bash
# cvenv installer.
#
# By default this walks you through the install: it detects what is available,
# asks about the usual settings (Enter keeps the suggested value), shows a summary
# and only then changes anything. Use --headless for scripted installs.
#
#   ./installation.sh [options]
#   curl -fsSL https://raw.githubusercontent.com/OWNER/cvenv/main/installation.sh | bash -s -- [options]
#
# Options (each one pre-answers the matching question)
#   --headless          no questions: use flags, current settings and detected defaults
#                       (aliases: --yes, -y, --non-interactive)
#   --version vX.Y.Z    release to install (default: latest)
#   --repo OWNER/NAME   GitHub repo with the releases (default: from `git remote origin`)
#   --url URL           download the binary from this URL
#   --binary PATH       install this local binary
#   --from-source       build from source with pixi (development; needs a git checkout)
#   --keep-binary       keep the already installed binary (just reconfigure)
#   --prefix DIR        install to DIR/bin/cvenv (default: the checkout, or ~/.cvenv)
#   --store DIR         default directory for new envs
#   --build-tmpdir DIR  node-local scratch for image builds (default /tmp)
#   --rebuild-mode M    where `cvenv rebuild` builds: local | slurm
#   --slurm-args ARGS   sbatch arguments for Slurm builds, e.g. "-A myacct -p mypart"
#   --modify-rc | --no-modify-rc   add cvenv to your shell rc (headless default: no)
#   --rc-file FILE      shell rc to modify (default ~/.bashrc, or ~/.zshrc for zsh)
#   --pixi PATH  --apptainer PATH  --no-pixi-install
#
# Safe to re-run (upgrades, new clusters, ...). Machine-local settings go to
# <prefix>/etc/install.conf, user settings to ~/.config/cvenv/config.json.
set -euo pipefail

# Fill this in once the GitHub repo exists, so `curl ... | bash` installs work
# without a checkout (a clone's `origin` remote is used automatically otherwise).
DEFAULT_GITHUB_REPO="${CVENV_GITHUB_REPO:-}"   # e.g. "OWNER/cvenv"

SRC_DIR=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    SRC_DIR="$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

# ---------------------------------------------------------------- arguments
HEADLESS=0
F_VERSION="" F_REPO="" F_URL="" F_BINARY="" F_SOURCE="" F_PREFIX="" F_STORE="" F_TMP=""
F_MODE="" F_SLURM="" F_SLURM_SET=0 F_RC="" F_RCFILE="" F_PIXI="" F_APPT="" PIXI_INSTALL=1
while [ $# -gt 0 ]; do
    case "$1" in
        --headless|--yes|-y|--non-interactive) HEADLESS=1; shift ;;
        --version) F_VERSION="$2"; shift 2 ;;
        --repo) F_REPO="$2"; shift 2 ;;
        --url) F_URL="$2"; F_SOURCE=url; shift 2 ;;
        --binary) F_BINARY="$2"; F_SOURCE=binary; shift 2 ;;
        --from-source) F_SOURCE=source; shift ;;
        --keep-binary) F_SOURCE=keep; shift ;;
        --prefix) F_PREFIX="$2"; shift 2 ;;
        --store) F_STORE="$2"; shift 2 ;;
        --store=*) F_STORE="${1#*=}"; shift ;;
        --build-tmpdir) F_TMP="$2"; shift 2 ;;
        --rebuild-mode) F_MODE="$2"; shift 2 ;;
        --slurm-args) F_SLURM="$2"; F_SLURM_SET=1; shift 2 ;;
        --modify-rc) F_RC=yes; shift ;;
        --no-modify-rc) F_RC=no; shift ;;
        --rc-file) F_RCFILE="$2"; shift 2 ;;
        --pixi) F_PIXI="$2"; shift 2 ;;
        --apptainer) F_APPT="$2"; shift 2 ;;
        --no-pixi-install) PIXI_INSTALL=0; shift ;;
        -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]:-$0}" 2>/dev/null || true; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done
[ -n "$F_VERSION$F_REPO" ] && [ -z "$F_SOURCE" ] && F_SOURCE=download
case "$F_MODE" in ""|local|slurm) ;; *) echo "--rebuild-mode must be local or slurm" >&2; exit 2 ;; esac

# ---------------------------------------------------------------- output / prompts
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
    B=$'\033[1m' DIM=$'\033[2m' BLUE=$'\033[1;34m' GREEN=$'\033[32m' YEL=$'\033[33m' RED=$'\033[1;31m' R=$'\033[0m'
else
    B="" DIM="" BLUE="" GREEN="" YEL="" RED="" R=""
fi
say()  { printf '%s==>%s %s%s%s\n' "$BLUE" "$R" "$B" "$*" "$R" >&2; }
ok()   { printf '    %s✔%s %s\n' "$GREEN" "$R" "$*" >&2; }
warn() { printf '    %s!%s %s\n' "$YEL" "$R" "$*" >&2; }
note() { printf '    %s%s%s\n' "$DIM" "$*" "$R" >&2; }
fail() { printf '%serror:%s %s\n' "$RED" "$R" "$*" >&2; exit 1; }

# Prompts read from the terminal, so they also work under `curl ... | bash`.
TTY=""
if [ "$HEADLESS" = 0 ]; then
    if { : </dev/tty >/dev/tty; } 2>/dev/null; then
        TTY=/dev/tty
    else
        HEADLESS=1
        printf 'no terminal available - running headless (see --help for flags)\n' >&2
    fi
fi

expand_path() {  # ~ and relative paths -> absolute
    local p="$1"
    case "$p" in "~") p="$HOME" ;; "~/"*) p="$HOME/${p#\~/}" ;; esac
    case "$p" in /*) ;; "") ;; *) p="$PWD/$p" ;; esac
    printf '%s' "$p"
}

ask_value() {  # ask_value "Question" "default" -> answer on stdout
    local a
    printf '  %s %s[%s]%s: ' "$1" "$DIM" "${2:-none}" "$R" >"$TTY"
    IFS= read -r a <"$TTY" || a=""
    if [ -z "$a" ]; then printf '%s' "$2"; elif [ "$a" = "-" ]; then printf ''; else printf '%s' "$a"; fi
}

ask_yn() {  # ask_yn "Question" y|n -> returns 0 for yes
    local a d="$2"
    printf '  %s %s[%s]%s ' "$1" "$DIM" "$([ "$d" = y ] && echo Y/n || echo y/N)" "$R" >"$TTY"
    IFS= read -r a <"$TTY" || a=""
    a="${a:-$d}"
    [[ "$a" =~ ^[Yy] ]]
}

ask_choice() {  # ask_choice "Question" default_key "key|description" ... -> key on stdout
    local q="$1" d="$2" i=1 a opt key
    shift 2
    printf '  %s\n' "$q" >"$TTY"
    for opt in "$@"; do
        key="${opt%%|*}"
        printf '    %s%d)%s %-9s %s%s\n' "$B" "$i" "$R" "$key" "${opt#*|}" "$([ "$key" = "$d" ] && echo "  (default)")" >"$TTY"
        i=$((i + 1))
    done
    while true; do
        printf '  choice %s[%s]%s: ' "$DIM" "$d" "$R" >"$TTY"
        IFS= read -r a <"$TTY" || a=""
        a="${a:-$d}"
        i=1
        for opt in "$@"; do
            key="${opt%%|*}"
            if [ "$a" = "$i" ] || [ "$a" = "$key" ]; then printf '%s' "$key"; return; fi
            i=$((i + 1))
        done
        printf '  please enter a number 1-%d\n' "$#" >"$TTY"
    done
}

fetch() {  # fetch URL DEST
    if command -v curl >/dev/null; then curl -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null; then wget -q -O "$2" "$1"
    else fail "need curl or wget to download"; fi
}

# ---------------------------------------------------------------- detection
[ "$(uname -s)" = Linux ] || fail "cvenv supports Linux only (Apptainer)"
case "$(uname -m)" in
    x86_64|amd64) TRIPLE=x86_64-unknown-linux-gnu ;;
    aarch64|arm64) TRIPLE=aarch64-unknown-linux-gnu ;;
    *) TRIPLE="" ;;
esac

IN_CHECKOUT=0
[ -n "$SRC_DIR" ] && [ -f "$SRC_DIR/Cargo.toml" ] && [ -f "$SRC_DIR/src/main.rs" ] && IN_CHECKOUT=1

CFG_DIR="${CVENV_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/cvenv}"
cfg_get() {  # current value from ~/.config/cvenv/config.json (flat string values)
    [ -f "$CFG_DIR/config.json" ] || return 0
    sed -n "s/^[[:space:]]*\"$1\"[[:space:]]*:[[:space:]]*\"\(.*\)\"[[:space:]]*,\{0,1\}[[:space:]]*\$/\1/p" "$CFG_DIR/config.json" | head -n1
}

D_PREFIX="${F_PREFIX:-$([ "$IN_CHECKOUT" = 1 ] && echo "$SRC_DIR" || echo "${CVENV_PREFIX:-$HOME/.cvenv}")}"
conf_get() {  # value from an existing <prefix>/etc/install.conf
    local f="$1/etc/install.conf"
    [ -f "$f" ] || return 0
    ( set +u; . "$f" >/dev/null 2>&1; eval "printf '%s' \"\${$2:-}\"" )
}

D_REPO="${F_REPO:-$DEFAULT_GITHUB_REPO}"
if [ -z "$D_REPO" ] && [ "$IN_CHECKOUT" = 1 ]; then
    origin="$(git -C "$SRC_DIR" remote get-url origin 2>/dev/null || true)"
    D_REPO="$(printf '%s' "$origin" | sed -nE 's#^(https?://|git@)github\.com[:/]([^/]+/[^/]+)$#\2#p' | sed 's/\.git$//')"
fi

detect_pixi() {
    local p="${F_PIXI:-$(command -v pixi || true)}"
    if [ -z "$p" ] && [ -x "${PIXI_HOME:-$HOME/.pixi}/bin/pixi" ]; then p="${PIXI_HOME:-$HOME/.pixi}/bin/pixi"; fi
    [ -n "$p" ] && readlink -f "$p" || true
}
detect_apptainer() {
    local p="${F_APPT:-$(command -v apptainer || command -v singularity || true)}" m
    if [ -z "$p" ]; then
        # HPC sites usually provide it as an environment module
        for m in apptainer singularity singularityce; do
            p="$(bash -lc "module load $m >/dev/null 2>&1 && { command -v apptainer || command -v singularity; }" 2>/dev/null | tail -n1 || true)"
            [ -n "$p" ] && break
        done
    fi
    printf '%s' "$p"
}

say "Checking this machine"
PIXI="$(detect_pixi)"
if [ -n "$PIXI" ]; then ok "pixi: $PIXI ($("$PIXI" --version 2>/dev/null))"; else warn "pixi: not found (can be installed)"; fi
APPT="$(detect_apptainer)"
if [ -n "$APPT" ]; then ok "apptainer: $APPT"; else warn "apptainer: not found (needed to build/run envs)"; fi
HAS_SLURM=0
if command -v sbatch >/dev/null; then HAS_SLURM=1; ok "Slurm: $(command -v sbatch)"; fi
[ "$IN_CHECKOUT" = 1 ] && ok "running from a checkout: $SRC_DIR"
if [ -x "$D_PREFIX/bin/cvenv" ] && "$D_PREFIX/bin/cvenv" --version >/dev/null 2>&1; then
    ok "installed: $D_PREFIX/bin/cvenv ($("$D_PREFIX/bin/cvenv" --version))"
fi

# ---------------------------------------------------------------- defaults
PREFIX="$D_PREFIX"
REPO_SLUG="$D_REPO"
VERSION="${F_VERSION:-latest}"
URL="$F_URL"
BINARY="$F_BINARY"
if [ -n "$F_SOURCE" ]; then SOURCE="$F_SOURCE"
elif [ -n "$REPO_SLUG" ]; then SOURCE=download
elif [ -x "$PREFIX/bin/cvenv" ] && "$PREFIX/bin/cvenv" --version >/dev/null 2>&1; then SOURCE=keep
elif [ "$IN_CHECKOUT" = 1 ]; then SOURCE=source
else SOURCE=download; fi
STORE="${F_STORE:-$(cfg_get store)}"; STORE="${STORE:-$HOME/.cvenv/envs}"
BUILD_TMP="${F_TMP:-$(conf_get "$PREFIX" CVENV_INSTALL_BUILD_TMPDIR)}"; BUILD_TMP="${BUILD_TMP:-${CVENV_BUILD_TMPDIR:-/tmp}}"
MODE="${F_MODE:-$(cfg_get rebuild-mode)}"; MODE="${MODE:-local}"
if [ "$F_SLURM_SET" = 1 ]; then SLURM_ARGS="$F_SLURM"; else SLURM_ARGS="$(cfg_get slurm-args)"; fi
RCFILE="${F_RCFILE:-$HOME/.bashrc}"
[ -z "$F_RCFILE" ] && [ "${SHELL##*/}" = zsh ] && RCFILE="$HOME/.zshrc"
if [ -n "$F_RC" ]; then MODIFY_RC="$F_RC"
elif [ "$HEADLESS" = 1 ]; then MODIFY_RC=no
elif grep -qF "# >>> cvenv >>>" "$RCFILE" 2>/dev/null; then MODIFY_RC=yes
else MODIFY_RC=yes; fi
INSTALL_PIXI=0
[ -z "$PIXI" ] && [ "$PIXI_INSTALL" = 1 ] && INSTALL_PIXI=1

# ---------------------------------------------------------------- walkthrough
wizard() {
    printf '\n%sSet up cvenv%s - press Enter to keep the value in [brackets], "-" to clear it.\n\n' "$B" "$R" >"$TTY"

    if [ -z "$F_PREFIX" ]; then
        printf '%s1. Install location%s  %s(bin/cvenv and etc/install.conf go here)%s\n' "$B" "$R" "$DIM" "$R" >"$TTY"
        PREFIX="$(expand_path "$(ask_value "Install prefix" "$PREFIX")")"
        echo >"$TTY"
    fi

    if [ -z "$F_SOURCE" ]; then
        # the prefix may have changed: "keep" only makes sense if a binary is there
        if [ "$SOURCE" = keep ] && [ ! -x "$PREFIX/bin/cvenv" ]; then
            if [ -n "$REPO_SLUG" ]; then SOURCE=download; elif [ "$IN_CHECKOUT" = 1 ]; then SOURCE=source; else SOURCE=download; fi
        fi
        printf '%s2. cvenv binary%s\n' "$B" "$R" >"$TTY"
        local opts=("download|prebuilt static binary from the GitHub release (recommended)")
        [ "$IN_CHECKOUT" = 1 ] && opts+=("source|build from this checkout with pixi (development)")
        opts+=("binary|install a binary file you already have")
        [ -x "$PREFIX/bin/cvenv" ] && opts+=("keep|keep the installed $("$PREFIX/bin/cvenv" --version 2>/dev/null || echo binary), only reconfigure")
        SOURCE="$(ask_choice "Where should the cvenv binary come from?" "$SOURCE" "${opts[@]}")"
        case "$SOURCE" in
            download)
                REPO_SLUG="$(ask_value "GitHub repo (OWNER/NAME)" "$REPO_SLUG")"
                VERSION="$(ask_value "Release version" "$VERSION")" ;;
            binary)
                BINARY="$(expand_path "$(ask_value "Path to the binary" "$BINARY")")" ;;
        esac
        echo >"$TTY"
    fi

    printf '%s3. Tools%s\n' "$B" "$R" >"$TTY"
    if [ -z "$F_PIXI" ]; then
        if [ -n "$PIXI" ]; then
            PIXI="$(expand_path "$(ask_value "pixi executable" "$PIXI")")"
            INSTALL_PIXI=0
        elif [ "$PIXI_INSTALL" = 1 ] && ask_yn "pixi was not found. Install it to ~/.pixi/bin (one binary)?" y; then
            INSTALL_PIXI=1
        else
            PIXI="$(expand_path "$(ask_value "pixi executable" "")")"
            INSTALL_PIXI=0
        fi
    fi
    if [ -z "$F_APPT" ]; then
        APPT="$(expand_path "$(ask_value "apptainer executable" "$APPT")")"
    fi
    echo >"$TTY"

    printf '%s4. Where envs and builds live%s\n' "$B" "$R" >"$TTY"
    if [ -z "$F_STORE" ]; then
        note "New envs (pixi.toml + one .sif image each) are created in the store;" 2>"$TTY"
        note "put it on a filesystem with space (images are 0.1-10 GB). Movable later." 2>"$TTY"
        STORE="$(expand_path "$(ask_value "Env store" "$STORE")")"
    fi
    if [ -z "$F_TMP" ]; then
        note "Image builds unpack thousands of temporary files here; it must be node-local" 2>"$TTY"
        note "disk (not NFS/GPFS/Lustre) - they are deleted after each build." 2>"$TTY"
        BUILD_TMP="$(expand_path "$(ask_value "Build scratch directory" "$BUILD_TMP")")"
    fi
    echo >"$TTY"

    if [ "$HAS_SLURM" = 1 ] && { [ -z "$F_MODE" ] || [ "$F_SLURM_SET" = 0 ]; }; then
        printf '%s5. Slurm builds%s  %s(heavy envs, e.g. PyTorch, are better built on a compute node)%s\n' "$B" "$R" "$DIM" "$R" >"$TTY"
        if [ -z "$F_MODE" ]; then
            MODE="$(ask_choice "Where should \`cvenv rebuild\` build by default?" "$MODE" \
                "local|on the current node (use \`cvenv rebuild --slurm\` per build)" \
                "slurm|submit a Slurm job (use \`cvenv rebuild --local\` per build)")"
        fi
        if [ "$F_SLURM_SET" = 0 ]; then
            note "sbatch arguments for build jobs, e.g. \"-A <account> -p <partition>\"" 2>"$TTY"
            note "(defaults already include --cpus-per-task=8 --mem=32G --time=02:00:00)" 2>"$TTY"
            SLURM_ARGS="$(ask_value "sbatch arguments" "$SLURM_ARGS")"
        fi
        echo >"$TTY"
    fi

    if [ -z "$F_RC" ]; then
        printf '%s6. Shell integration%s  %s(adds cvenv to PATH and enables `cvenv activate`)%s\n' "$B" "$R" "$DIM" "$R" >"$TTY"
        if ask_yn "Add the cvenv block to your shell rc?" "$([ "$MODIFY_RC" = yes ] && echo y || echo n)"; then
            MODIFY_RC=yes
            [ -z "$F_RCFILE" ] && RCFILE="$(expand_path "$(ask_value "Shell rc file" "$RCFILE")")"
        else
            MODIFY_RC=no
        fi
        echo >"$TTY"
    fi
}

summary() {
    local src
    case "$SOURCE" in
        download) src="download ${VERSION} from github.com/${REPO_SLUG:-?}${URL:+ ($URL)}" ;;
        url) src="download $URL" ;;
        source) src="build from source ($SRC_DIR, pixi run build)" ;;
        binary) src="copy $BINARY" ;;
        keep) src="keep the installed binary" ;;
    esac
    printf '%sSummary%s\n' "$B" "$R" >&2
    printf '  %-22s %s\n' \
        "install prefix" "$PREFIX" \
        "cvenv binary" "$src" \
        "pixi" "$([ "$INSTALL_PIXI" = 1 ] && echo "install to ~/.pixi/bin" || echo "${PIXI:-(none)}")" \
        "apptainer" "${APPT:-(none - envs cannot be built/run)}" \
        "env store" "$STORE" \
        "build scratch" "$BUILD_TMP" >&2
    if [ "$HAS_SLURM" = 1 ] || [ "$MODE" = slurm ] || [ -n "$SLURM_ARGS" ]; then
        printf '  %-22s %s\n' "rebuild mode" "$MODE" "sbatch arguments" "${SLURM_ARGS:-(none)}" >&2
    fi
    printf '  %-22s %s\n' "shell rc" "$([ "$MODIFY_RC" = yes ] && echo "add/update block in $RCFILE" || echo "leave alone")" >&2
}

validate() {
    case "$SOURCE" in
        download) [ -n "$URL" ] || [ -n "$REPO_SLUG" ] || { warn "no GitHub repo given for the download"; return 1; }
                  [ -n "$URL" ] || [ -n "$TRIPLE" ] || { warn "no prebuilt binary for $(uname -m); build from source"; return 1; } ;;
        binary) [ -f "$BINARY" ] || { warn "binary not found: $BINARY"; return 1; } ;;
        source) [ "$IN_CHECKOUT" = 1 ] || { warn "building from source needs a git checkout"; return 1; } ;;
        keep) [ -x "$PREFIX/bin/cvenv" ] || { warn "no installed binary in $PREFIX/bin"; return 1; } ;;
    esac
    [ "$INSTALL_PIXI" = 1 ] || [ -x "$PIXI" ] || { warn "pixi executable not found: ${PIXI:-(none)}"; return 1; }
    if [ -n "$APPT" ] && [ ! -x "$APPT" ]; then warn "apptainer not executable: $APPT"; return 1; fi
    mkdir -p "$BUILD_TMP" 2>/dev/null && [ -w "$BUILD_TMP" ] || { warn "build scratch not writable: $BUILD_TMP"; return 1; }
    return 0
}

if [ "$HEADLESS" = 1 ]; then
    echo >&2; summary
    if ! validate; then
        fail "cannot continue headless; fix the flags above (e.g. --repo OWNER/NAME, --from-source, --binary PATH)"
    fi
else
    while true; do
        wizard
        summary
        if ! validate; then
            ask_yn "Some answers need fixing. Go through the questions again?" y && { echo >"$TTY"; continue; }
            fail "aborted"
        fi
        echo >"$TTY"
        a="$(ask_choice "Proceed?" yes "yes|install with these settings" "edit|go through the questions again" "abort|quit without changing anything")"
        case "$a" in
            yes) break ;;
            edit) echo >"$TTY" ;;
            abort) echo "aborted; nothing was changed" >&2; exit 1 ;;
        esac
    done
fi
echo >&2

# ================================================================= apply
mkdir -p "$PREFIX/bin" "$PREFIX/etc"
PREFIX="$(cd -P "$PREFIX" && pwd)"
CVENV="$PREFIX/bin/cvenv"
CONF="$PREFIX/etc/install.conf"

# node-local scratch for every cache used below - never $HOME or the repo
SCRATCH="$BUILD_TMP/cvenv-${USER:-$(id -u)}"
mkdir -p "$SCRATCH" && chmod 700 "$SCRATCH"
fstype="$(stat -f -c %T "$BUILD_TMP" 2>/dev/null || echo '?')"
case "$fstype" in
    nfs*|gpfs|lustre|beegfs*|ceph*) warn "$BUILD_TMP is a shared filesystem ($fstype); builds will be slow and create many files there" ;;
esac
export PIXI_CACHE_DIR="${PIXI_CACHE_DIR:-$SCRATCH/pixi-cache}"
export CVENV_BUILD_TMPDIR="$BUILD_TMP"

if [ "$INSTALL_PIXI" = 1 ]; then
    say "Installing pixi"
    command -v curl >/dev/null || fail "curl is needed to install pixi"
    curl -fsSL https://pixi.sh/install.sh | PIXI_NO_PATH_UPDATE=1 bash
    PIXI="${PIXI_HOME:-$HOME/.pixi}/bin/pixi"
    [ -x "$PIXI" ] || fail "pixi installation failed"
    ok "$PIXI ($("$PIXI" --version))"
fi
PIXI="$(readlink -f "$PIXI")"

say "Installing the cvenv binary"
install_bin() {  # install_bin FILE
    chmod 755 "$1"
    "$1" --version >/dev/null 2>&1 || { rm -f "$1"; fail "$1 does not run on this machine"; }
    mv -f "$1" "$CVENV"
}
case "$SOURCE" in
    source)
        # The Rust toolchain comes from the checkout's pixi.toml. Its env is detached to
        # node-local scratch (thousands of files); only the final binary lands in bin/.
        ( cd "$SRC_DIR" && "$PIXI" config set --local detached-environments "$SCRATCH/pixi-envs" >/dev/null )
        note "toolchain env: $SCRATCH/pixi-envs, caches: $SCRATCH"
        ( cd "$SRC_DIR" && "$PIXI" run build ) || fail "building cvenv failed"
        if [ "$(readlink -f "$SRC_DIR/bin/cvenv")" != "$(readlink -f "$CVENV" 2>/dev/null || echo x)" ]; then
            cp "$SRC_DIR/bin/cvenv" "$CVENV.new" && install_bin "$CVENV.new"
        fi ;;
    binary)
        cp "$BINARY" "$CVENV.new" && install_bin "$CVENV.new" ;;
    keep)
        "$CVENV" --version >/dev/null 2>&1 || fail "$CVENV does not run" ;;
    download|url)
        if [ -z "$URL" ]; then
            if [ "$VERSION" = latest ]; then
                URL="https://github.com/$REPO_SLUG/releases/latest/download/cvenv-$TRIPLE"
            else
                URL="https://github.com/$REPO_SLUG/releases/download/$VERSION/cvenv-$TRIPLE"
            fi
        fi
        note "downloading $URL"
        fetch "$URL" "$CVENV.new" || { rm -f "$CVENV.new"; fail "download failed ($URL). No release yet? Re-run and build from source."; }
        if fetch "$URL.sha256" "$CVENV.sha256" 2>/dev/null; then
            want="$(awk '{print $1}' "$CVENV.sha256")"; got="$(sha256sum "$CVENV.new" | awk '{print $1}')"
            rm -f "$CVENV.sha256"
            [ "$want" = "$got" ] || { rm -f "$CVENV.new"; fail "checksum mismatch for $URL"; }
            ok "checksum verified"
        else
            warn "no checksum published next to the binary; skipped verification"
        fi
        install_bin "$CVENV.new" ;;
esac
ok "$CVENV ($("$CVENV" --version))"

say "Writing settings"
APPT_CONF=""
if [ -n "$APPT" ]; then
    APPT_CONF="$("$APPT" buildcfg 2>/dev/null | sed -n 's/^\(APPTAINER\|SINGULARITY\)_CONF_FILE=//p' | head -n1 || true)"
    [ -n "$APPT_CONF" ] && [ -f "$APPT_CONF" ] || APPT_CONF=""
    n="$(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo 0)"
    [ "$n" -gt 0 ] 2>/dev/null || warn "user namespaces look disabled: image builds may need fakeroot/subuid setup or a build node"
fi
cat > "$CONF" <<EOF
# Written by installation.sh on $(hostname) at $(date). Machine-local; do not commit.
# Precedence: CVENV_* env vars > ~/.config/cvenv/config.json (cvenv config set) > this file.
export CVENV_INSTALL_PIXI=$(printf '%q' "$PIXI")
export CVENV_INSTALL_APPTAINER=$(printf '%q' "$APPT")
export CVENV_INSTALL_APPTAINER_CONF=$(printf '%q' "$APPT_CONF")
export CVENV_INSTALL_BUILD_TMPDIR=$(printf '%q' "$BUILD_TMP")
EOF
ok "machine settings: $CONF"
# user settings (stored in the cvenv config; flags/answers above win over old values)
cfg() {  # cfg set|unset KEY [VALUE] - quiet unless it fails
    local out
    out="$("$CVENV" config "$@" 2>&1)" || fail "cvenv config $*: $out"
}
cfg set store "$STORE" && ok "store = $STORE"
mkdir -p "$STORE"
cfg set rebuild-mode "$MODE" && ok "rebuild-mode = $MODE"
if [ -n "$SLURM_ARGS" ]; then
    cfg set slurm-args "$SLURM_ARGS" && ok "slurm-args = $SLURM_ARGS"
else
    cfg unset slurm-args
    [ "$MODE" = slurm ] && warn "rebuild-mode is slurm but no sbatch arguments are set; your cluster may require -A/-p"
fi
# an explicit build-tmpdir in the user config would shadow install.conf: keep them in sync
if [ -n "$(cfg_get build-tmpdir)" ]; then cfg set build-tmpdir "$BUILD_TMP"; fi

say "Shell integration"
BLOCK_START="# >>> cvenv >>>"
BLOCK_END="# <<< cvenv <<<"
block="$BLOCK_START
export PATH=\"$PREFIX/bin:\$PATH\"
eval \"\$(\"$CVENV\" shell-init)\"
$BLOCK_END"
if [ "$MODIFY_RC" = yes ]; then
    touch "$RCFILE"
    if grep -qF "$BLOCK_START" "$RCFILE"; then
        # replace the existing block (the install may have moved)
        tmp="$(mktemp)"
        awk -v s="$BLOCK_START" -v e="$BLOCK_END" '$0==s{skip=1} !skip{print} $0==e{skip=0}' "$RCFILE" > "$tmp"
        printf '%s\n' "$block" >> "$tmp"
        cat "$tmp" > "$RCFILE"; rm -f "$tmp"
        ok "updated the cvenv block in $RCFILE"
    else
        printf '\n%s\n' "$block" >> "$RCFILE"
        ok "added a cvenv block to $RCFILE"
    fi
    note "open a new shell (or \`source $RCFILE\`) to start using it"
else
    note "shell rc left alone. To use cvenv, add this to your shell rc:"
    printf '%s\n' "$block" | sed 's/^/        /' >&2
fi

say "Done"
env -u CVENV_BUILD_TMPDIR -u PIXI_CACHE_DIR "$CVENV" config | sed 's/^/    /' >&2
printf '\n  Get started:  cvenv init myenv python=3.12 numpy  &&  cvenv activate myenv  &&  cvenv rebuild\n' >&2
