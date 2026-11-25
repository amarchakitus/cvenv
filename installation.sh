#!/usr/bin/env bash
# cvenv installer
#
#   ./installation.sh [options]
#   curl -fsSL https://raw.githubusercontent.com/amarchakitus/cvenv/main/installation.sh | bash
#
# Detects pixi, Apptainer, squashfuse and Slurm, asks where envs should live (plus sbatch
# arguments on Slurm clusters, and whether to set up your shell), shows the plan and installs
# after confirmation. Anything missing is installed into your home directory; no root needed.
# Safe to re-run: it upgrades cvenv and keeps your settings.
#
# Options
#   -y, --yes             no questions: use flags, current settings and defaults
#   --store DIR           where envs are created (default ~/.cvenv/envs)
#   --slurm-args ARGS     sbatch arguments for build jobs, e.g. "-A myacct -p mypart"
#   --modify-rc, --no-modify-rc
#                         add cvenv to your shell rc (default: ask; with --yes: only
#                         update an existing cvenv block)
#   -h, --help            show this help
#
# Advanced
#   --prefix DIR          install location of bin/cvenv (default: this checkout, or ~/.cvenv)
#   --version vX.Y.Z      release to install (default: latest)
#   --repo OWNER/NAME     GitHub repo with the releases (default: amarchakitus/cvenv)
#   --url URL, --binary FILE, --from-source, --keep-binary
#                         where the cvenv binary comes from (default: download the release);
#                         downloads (also --url) are verified against URL.sha256
#   --backend B           image backend: auto (default), apptainer or squashfuse
#   --pixi PATH, --apptainer PATH, --squashfuse PATH
#                         use these tools instead of the detected ones
#   --no-pixi-install, --no-squashfuse-install
#                         don't install missing tools
#   --build-tmpdir DIR    node-local scratch for builds (default /tmp)
#   --rebuild-mode MODE   where `cvenv rebuild` builds by default: local or slurm
#   --rc-file FILE        shell rc to modify (default ~/.bashrc, or ~/.zshrc for zsh)
#
# Machine settings go to <prefix>/etc/install.conf, user settings to ~/.config/cvenv/config.json.
set -euo pipefail

DEFAULT_GITHUB_REPO="${CVENV_GITHUB_REPO:-amarchakitus/cvenv}"

SRC_DIR=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    SRC_DIR="$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

usage() {
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]:-$0}" 2>/dev/null \
        || echo "see https://github.com/$DEFAULT_GITHUB_REPO"
}

# ---------------------------------------------------------------- arguments
YES=0
F_VERSION="" F_REPO="" F_URL="" F_BINARY="" F_SOURCE="" F_PREFIX="" F_STORE="" F_TMP=""
F_MODE="" F_SLURM="" F_SLURM_SET=0 F_RC="" F_RCFILE="" F_PIXI="" F_APPT="" F_SQFS="" F_BACKEND=""
PIXI_INSTALL=1 SQFS_INSTALL=1
need() { [ $# -ge 2 ] || { echo "option $1 needs a value (see --help)" >&2; exit 2; }; }
while [ $# -gt 0 ]; do
    case "$1" in
        -y|--yes|--headless|--non-interactive) YES=1; shift ;;
        --store) need "$@"; F_STORE="$2"; shift 2 ;;
        --store=*) F_STORE="${1#*=}"; shift ;;
        --slurm-args) need "$@"; F_SLURM="$2"; F_SLURM_SET=1; shift 2 ;;
        --modify-rc) F_RC=yes; shift ;;
        --no-modify-rc) F_RC=no; shift ;;
        --prefix) need "$@"; F_PREFIX="$2"; shift 2 ;;
        --version) need "$@"; F_VERSION="$2"; shift 2 ;;
        --repo) need "$@"; F_REPO="$2"; shift 2 ;;
        --url) need "$@"; F_URL="$2"; F_SOURCE=url; shift 2 ;;
        --binary) need "$@"; F_BINARY="$2"; F_SOURCE=binary; shift 2 ;;
        --from-source) F_SOURCE=source; shift ;;
        --keep-binary) F_SOURCE=keep; shift ;;
        --backend) need "$@"; F_BACKEND="$2"; shift 2 ;;
        --pixi) need "$@"; F_PIXI="$2"; shift 2 ;;
        --apptainer) need "$@"; F_APPT="$2"; shift 2 ;;
        --squashfuse) need "$@"; F_SQFS="$2"; shift 2 ;;
        --no-pixi-install) PIXI_INSTALL=0; shift ;;
        --no-squashfuse-install) SQFS_INSTALL=0; shift ;;
        --build-tmpdir) need "$@"; F_TMP="$2"; shift 2 ;;
        --rebuild-mode) need "$@"; F_MODE="$2"; shift 2 ;;
        --rc-file) need "$@"; F_RCFILE="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done
case "$F_MODE" in ""|local|slurm) ;; *) echo "--rebuild-mode must be local or slurm" >&2; exit 2 ;; esac
case "$F_BACKEND" in ""|auto|apptainer|squashfuse) ;; *) echo "--backend must be auto, apptainer or squashfuse" >&2; exit 2 ;; esac

# ---------------------------------------------------------------- output / prompts
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
    B=$'\033[1m' DIM=$'\033[2m' BLUE=$'\033[1;34m' GREEN=$'\033[32m' YEL=$'\033[33m' RED=$'\033[1;31m' R=$'\033[0m'
else
    B="" DIM="" BLUE="" GREEN="" YEL="" RED="" R=""
fi
section() { printf '\n%s%s%s\n' "$B" "$*" "$R" >&2; }
found()   { printf '  %s✔%s %-11s %s\n' "$GREEN" "$R" "$1" "$2" >&2; }
missing() { printf '  %s-%s %-11s %s%s%s\n' "$DIM" "$R" "$1" "$DIM" "$2" "$R" >&2; }
row()     { printf '  %-11s %s\n' "$1" "$2" >&2; }
step()    { printf '%s==>%s %s\n' "$BLUE" "$R" "$*" >&2; }
warn()    { printf '  %s! %s%s\n' "$YEL" "$*" "$R" >&2; }
fail()    { printf '%serror:%s %s\n' "$RED" "$R" "$*" >&2; exit 1; }

# Prompts read from the terminal, so they also work under `curl ... | bash`.
TTY=""
if [ "$YES" = 0 ]; then
    if { : </dev/tty >/dev/tty; } 2>/dev/null; then
        TTY=/dev/tty
    else
        YES=1
        printf 'No terminal available: installing with defaults (see --help for options).\n' >&2
    fi
fi

ask_value() {  # ask_value "Question" "default" -> answer on stdout ("-" clears)
    local a
    printf '  %s %s[%s]%s: ' "$1" "$DIM" "${2:-none}" "$R" >"$TTY"
    IFS= read -r a <"$TTY" || a=""
    if [ -z "$a" ]; then printf '%s' "$2"; elif [ "$a" = "-" ]; then printf ''; else printf '%s' "$a"; fi
}

ask_yn() {  # ask_yn "Question" y|n -> returns 0 for yes
    local a d="$2"
    printf '  %s %s[%s]%s ' "$1" "$DIM" "$([ "$d" = y ] && echo Y/n || echo y/N)" "$R" >"$TTY"
    IFS= read -r a <"$TTY" || a=""
    [[ "${a:-$d}" =~ ^[Yy] ]]
}

expand_path() {  # ~ and relative paths -> absolute
    local p="$1"
    # shellcheck disable=SC2088  # matching a literal "~" typed by the user
    case "$p" in "~") p="$HOME" ;; "~/"*) p="$HOME/${p#\~/}" ;; esac
    case "$p" in /*|"") ;; *) p="$PWD/$p" ;; esac
    printf '%s' "$p"
}

# shellcheck disable=SC2088  # printing a literal "~" for display
tildify() { case "$1" in "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;; *) printf '%s' "$1" ;; esac; }

fetch() {  # fetch URL DEST
    if command -v curl >/dev/null; then curl -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null; then wget -q -O "$2" "$1"
    else fail "curl or wget is needed to download"; fi
}

# ---------------------------------------------------------------- detection
[ "$(uname -s)" = Linux ] || fail "cvenv supports Linux only"
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

PREFIX="$(expand_path "${F_PREFIX:-$([ "$IN_CHECKOUT" = 1 ] && echo "$SRC_DIR" || echo "${CVENV_PREFIX:-$HOME/.cvenv}")}")"
conf_get() {  # value from an existing <prefix>/etc/install.conf
    local f="$PREFIX/etc/install.conf"
    [ -f "$f" ] || return 0
    # shellcheck disable=SC1090  # install.conf path is only known at runtime
    ( set +u; . "$f" >/dev/null 2>&1; eval "printf '%s' \"\${$1:-}\"" )
}

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
detect_squashfuse() {
    local p="${F_SQFS:-$(conf_get CVENV_INSTALL_SQUASHFUSE)}"
    [ -n "$p" ] && [ -x "$p" ] && { printf '%s' "$p"; return; }
    p="$(command -v squashfuse_ll || command -v squashfuse || true)"
    [ -z "$p" ] && [ -x "$PREFIX/opt/squashfuse/usr/bin/squashfuse_ll" ] && p="$PREFIX/opt/squashfuse/usr/bin/squashfuse_ll"
    printf '%s' "$p"
}
fuse_ok() { [ -e /dev/fuse ] && { command -v fusermount3 >/dev/null || command -v fusermount >/dev/null; }; }
can_fetch_squashfuse() { command -v apt-get >/dev/null && command -v dpkg-deb >/dev/null && apt-cache show squashfuse >/dev/null 2>&1; }

PIXI="$(detect_pixi)"
APPT="$(detect_apptainer)"
SQFS="$(detect_squashfuse)"
HAS_SLURM=0; command -v sbatch >/dev/null && HAS_SLURM=1
INSTALLED=""
[ -x "$PREFIX/bin/cvenv" ] && INSTALLED="$("$PREFIX/bin/cvenv" --version 2>/dev/null || true)"

# ---------------------------------------------------------------- decisions (no questions)
REPO_SLUG="${F_REPO:-$DEFAULT_GITHUB_REPO}"
VERSION="${F_VERSION:-latest}"
URL="$F_URL"
BINARY="$(expand_path "$F_BINARY")"
SOURCE="${F_SOURCE:-download}"
BACKEND="${F_BACKEND:-$(cfg_get backend)}"; BACKEND="${BACKEND:-auto}"
BUILD_TMP="$(expand_path "${F_TMP:-$(conf_get CVENV_INSTALL_BUILD_TMPDIR)}")"; BUILD_TMP="${BUILD_TMP:-${CVENV_BUILD_TMPDIR:-/tmp}}"
MODE="${F_MODE:-$(cfg_get rebuild-mode)}"; MODE="${MODE:-local}"
STORE="$(expand_path "${F_STORE:-$(cfg_get store)}")"; STORE="${STORE:-$HOME/.cvenv/envs}"
if [ "$F_SLURM_SET" = 1 ]; then SLURM_ARGS="$F_SLURM"; else SLURM_ARGS="$(cfg_get slurm-args)"; fi
RCFILE="$(expand_path "${F_RCFILE:-$HOME/.bashrc}")"
[ -z "$F_RCFILE" ] && [ "${SHELL##*/}" = zsh ] && RCFILE="$HOME/.zshrc"
RC_HAS_BLOCK=0; grep -qF "# >>> cvenv >>>" "$RCFILE" 2>/dev/null && RC_HAS_BLOCK=1
# with --yes, a block added earlier is kept up to date (it may point at an old location)
if [ -n "$F_RC" ]; then MODIFY_RC="$F_RC"; elif [ "$YES" = 1 ]; then MODIFY_RC=$([ "$RC_HAS_BLOCK" = 1 ] && echo yes || echo no); else MODIFY_RC=ask; fi

INSTALL_PIXI=0
if [ -z "$PIXI" ]; then
    [ "$PIXI_INSTALL" = 1 ] || fail "pixi was not found; install it or pass --pixi PATH"
    INSTALL_PIXI=1
fi
INSTALL_SQFS=0
if [ -z "$SQFS" ] && [ "$SQFS_INSTALL" = 1 ] && fuse_ok && can_fetch_squashfuse; then
    if { [ -z "$APPT" ] && [ "$BACKEND" != apptainer ]; } || [ "$BACKEND" = squashfuse ]; then INSTALL_SQFS=1; fi
fi

# what builds will use
if [ "$BACKEND" = apptainer ] || { [ "$BACKEND" = auto ] && [ -n "$APPT" ]; }; then
    USES=apptainer
elif [ -n "$SQFS" ] || [ "$INSTALL_SQFS" = 1 ]; then
    USES=squashfuse
else
    USES=none
fi

# fail early on impossible flag combinations
case "$SOURCE" in
    download) [ -n "$REPO_SLUG" ] || fail "no GitHub repo for the download (--repo OWNER/NAME)"
              [ -n "$TRIPLE" ] || [ "$IN_CHECKOUT" = 1 ] || fail "no prebuilt binary for $(uname -m); use --from-source in a checkout" ;;
    binary) [ -f "$BINARY" ] || fail "binary not found: $BINARY" ;;
    source) [ "$IN_CHECKOUT" = 1 ] || fail "--from-source needs a git checkout of cvenv" ;;
    keep) [ -n "$INSTALLED" ] || fail "--keep-binary: no working cvenv in $PREFIX/bin" ;;
esac
[ -z "$APPT" ] || [ -x "$APPT" ] || fail "apptainer not executable: $APPT"
[ -z "$SQFS" ] || [ -x "$SQFS" ] || fail "squashfuse not executable: $SQFS"
mkdir -p "$BUILD_TMP" 2>/dev/null && [ -w "$BUILD_TMP" ] || fail "build scratch not writable: $BUILD_TMP (--build-tmpdir DIR)"

# ---------------------------------------------------------------- what we found
printf '%scvenv installer%s\n' "$B" "$R" >&2
section "Detected"
if [ -n "$PIXI" ]; then found pixi "$(tildify "$PIXI") ($("$PIXI" --version 2>/dev/null | awk '{print $2}'))"; else missing pixi "not found (will be installed to ~/.pixi/bin)"; fi
if [ -n "$APPT" ]; then found apptainer "$APPT"; else missing apptainer "not found"; fi
if [ -n "$SQFS" ]; then found squashfuse "$(tildify "$SQFS")"
elif [ "$INSTALL_SQFS" = 1 ]; then missing squashfuse "not found (will be fetched, no root needed)"
else missing squashfuse "not found"; fi
if [ "$HAS_SLURM" = 1 ]; then found Slurm "$(command -v sbatch)"; fi
case "$USES" in
    apptainer) found backend "apptainer$([ "$BACKEND" = auto ] && echo " (auto)")" ;;
    squashfuse) found backend "squashfuse$([ "$BACKEND" = auto ] && echo " (auto: no Apptainer)")" ;;
    none) warn "no backend: install Apptainer, or squashfuse with FUSE, to build envs" ;;
esac
[ -n "$INSTALLED" ] && found installed "$(tildify "$PREFIX/bin/cvenv") ($INSTALLED)"

# ---------------------------------------------------------------- the three questions
if [ "$YES" = 0 ]; then
    echo >"$TTY"
    [ -n "$F_STORE" ] || STORE="$(expand_path "$(ask_value "Where should envs be stored?" "$(tildify "$STORE")")")"
    if [ "$HAS_SLURM" = 1 ] && [ "$F_SLURM_SET" = 0 ]; then
        SLURM_ARGS="$(ask_value "sbatch arguments for build jobs (e.g. -A acct -p part)" "$SLURM_ARGS")"
    fi
    if [ "$MODIFY_RC" = ask ]; then
        if ask_yn "$([ "$RC_HAS_BLOCK" = 1 ] && echo Update || echo Add) cvenv in $(tildify "$RCFILE")?" y; then MODIFY_RC=yes; else MODIFY_RC=no; fi
    fi
fi
[ -n "$STORE" ] || fail "the env store can't be empty"

# ---------------------------------------------------------------- plan
case "$SOURCE" in
    download) SRC_DESC="$([ "$VERSION" = latest ] && echo "latest release" || echo "release $VERSION")$([ "$REPO_SLUG" = "$DEFAULT_GITHUB_REPO" ] || echo " of $REPO_SLUG")" ;;
    url) SRC_DESC="from $URL" ;;
    binary) SRC_DESC="copy of $BINARY" ;;
    source) SRC_DESC="built from this checkout" ;;
    keep) SRC_DESC="keep $INSTALLED" ;;
esac
section "Plan"
row install "$(tildify "$PREFIX/bin/cvenv") ($SRC_DESC)"
[ "$INSTALL_PIXI" = 1 ] && row pixi "install to ~/.pixi/bin"
[ "$INSTALL_SQFS" = 1 ] && row squashfuse "fetch into $(tildify "$PREFIX/opt/squashfuse")"
row store "$(tildify "$STORE")"
if [ "$HAS_SLURM" = 1 ] || [ "$MODE" = slurm ] || [ -n "$SLURM_ARGS" ]; then
    if [ "$MODE" = slurm ]; then row slurm "builds run as jobs: sbatch ${SLURM_ARGS:-(no extra arguments)}"
    else row slurm "\`cvenv rebuild --slurm\` uses: sbatch ${SLURM_ARGS:-(no extra arguments)}"; fi
fi
case "$MODIFY_RC" in
    yes) row "shell rc" "$([ "$RC_HAS_BLOCK" = 1 ] && echo update || echo add to) $(tildify "$RCFILE")" ;;
    *) row "shell rc" "leave alone" ;;
esac
case "$(stat -f -c %T "$BUILD_TMP" 2>/dev/null || echo '?')" in
    nfs*|gpfs|lustre|beegfs*|ceph*) warn "$BUILD_TMP is a shared filesystem: builds will be slow there (--build-tmpdir DIR)" ;;
esac
[ "$MODE" = slurm ] && [ -z "$SLURM_ARGS" ] && warn "builds run as Slurm jobs, but no sbatch arguments are set; your cluster may need -A/-p"

if [ "$YES" = 0 ]; then
    echo >"$TTY"
    ask_yn "Proceed?" y || { echo "Nothing was changed." >&2; exit 1; }
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
export PIXI_CACHE_DIR="${PIXI_CACHE_DIR:-$SCRATCH/pixi-cache}"
export CVENV_BUILD_TMPDIR="$BUILD_TMP"

if [ "$INSTALL_PIXI" = 1 ]; then
    step "Installing pixi"
    command -v curl >/dev/null || fail "curl is needed to install pixi"
    curl -fsSL https://pixi.sh/install.sh | PIXI_NO_PATH_UPDATE=1 bash >/dev/null
    PIXI="${PIXI_HOME:-$HOME/.pixi}/bin/pixi"
    [ -x "$PIXI" ] || fail "pixi installation failed"
fi
PIXI="$(readlink -f "$PIXI")"

if [ "$INSTALL_SQFS" = 1 ]; then
    # The distro's squashfuse plus the libraries this machine lacks, unpacked into the
    # prefix: `apt-get download` and `dpkg-deb -x` need no root (cvenv sets LD_LIBRARY_PATH).
    step "Fetching squashfuse"
    SQ_DIR="$PREFIX/opt/squashfuse"
    sq_tmp="$(mktemp -d "$SCRATCH/squashfuse-XXXXXX")"
    sq_pkgs="squashfuse"
    for p in $(apt-cache depends squashfuse 2>/dev/null | awk '/Depends:/ {print $2}' | tr -d '<>' | sort -u); do
        dpkg -s "$p" >/dev/null 2>&1 || sq_pkgs="$sq_pkgs $p"
    done
    # shellcheck disable=SC2086  # $sq_pkgs is a space-separated package list
    ( cd "$sq_tmp" && apt-get download $sq_pkgs >/dev/null 2>&1 ) || { rm -rf "$sq_tmp"; fail "apt-get download $sq_pkgs failed"; }
    rm -rf "$SQ_DIR"; mkdir -p "$SQ_DIR"
    for f in "$sq_tmp"/*.deb; do dpkg-deb -x "$f" "$SQ_DIR"; done
    rm -rf "$sq_tmp"
    SQFS="$SQ_DIR/usr/bin/squashfuse_ll"
    triple="$(uname -m)-linux-gnu"
    sq_out="$(LD_LIBRARY_PATH="$SQ_DIR/usr/lib/$triple:$SQ_DIR/lib/$triple${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$SQFS" --help 2>&1 || true)"
    case "$sq_out" in *squashfuse*) ;; *) fail "the fetched squashfuse does not run ($SQFS): $sq_out" ;; esac
fi

install_bin() {  # install_bin FILE
    chmod 755 "$1"
    "$1" --version >/dev/null 2>&1 || { rm -f "$1"; fail "$1 does not run on this machine"; }
    mv -f "$1" "$CVENV"
}
build_from_source() {
    # The Rust toolchain comes from the checkout's pixi.toml. Its env is detached to
    # node-local scratch (thousands of files); only the final binary lands in bin/.
    ( cd "$SRC_DIR" && "$PIXI" config set --local detached-environments "$SCRATCH/pixi-envs" >/dev/null )
    ( cd "$SRC_DIR" && "$PIXI" run build >&2 ) || fail "building cvenv failed"
    if [ "$(readlink -f "$SRC_DIR/bin/cvenv")" != "$(readlink -f "$CVENV" 2>/dev/null || echo x)" ]; then
        cp "$SRC_DIR/bin/cvenv" "$CVENV.new" && install_bin "$CVENV.new"
    fi
}
case "$SOURCE" in
    source)
        step "Building cvenv from source"
        build_from_source ;;
    binary)
        step "Installing cvenv"
        cp "$BINARY" "$CVENV.new" && install_bin "$CVENV.new" ;;
    keep)
        "$CVENV" --version >/dev/null 2>&1 || fail "$CVENV does not run" ;;
    download|url)
        step "Downloading cvenv"
        if [ -z "$URL" ]; then
            if [ "$VERSION" = latest ]; then
                URL="https://github.com/$REPO_SLUG/releases/latest/download/cvenv-$TRIPLE"
            else
                URL="https://github.com/$REPO_SLUG/releases/download/$VERSION/cvenv-$TRIPLE"
            fi
        fi
        if [ -n "$TRIPLE$F_URL" ] && fetch "$URL" "$CVENV.new" 2>/dev/null; then
            # like `cvenv self-update`: never install a download without a matching checksum
            # (to install an unverified file anyway, download it yourself and use --binary FILE)
            if ! fetch "$URL.sha256" "$CVENV.sha256" 2>/dev/null; then
                rm -f "$CVENV.new" "$CVENV.sha256"
                fail "no checksum at $URL.sha256; refusing to install an unverified binary (to install it anyway, download it yourself and use --binary FILE)"
            fi
            want="$(awk '{print tolower($1); exit}' "$CVENV.sha256")"; got="$(sha256sum "$CVENV.new" | awk '{print $1}')"
            rm -f "$CVENV.sha256"
            [ "$want" = "$got" ] || { rm -f "$CVENV.new"; fail "checksum mismatch for $URL (expected ${want:-nothing}, got $got)"; }
            install_bin "$CVENV.new"
        elif [ "$SOURCE" = download ] && [ -z "$F_SOURCE$F_VERSION$F_REPO" ] && [ "$IN_CHECKOUT" = 1 ]; then
            rm -f "$CVENV.new"
            warn "download failed ($URL); building from this checkout instead"
            build_from_source
        else
            rm -f "$CVENV.new"
            fail "download failed ($URL)"
        fi ;;
esac

step "Saving settings"
APPT_CONF=""
if [ -n "$APPT" ]; then
    APPT_CONF="$("$APPT" buildcfg 2>/dev/null | sed -n 's/^\(APPTAINER\|SINGULARITY\)_CONF_FILE=//p' | head -n1 || true)"
    [ -n "$APPT_CONF" ] && [ -f "$APPT_CONF" ] || APPT_CONF=""
    n="$(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo 0)"
    [ "$n" -gt 0 ] 2>/dev/null || warn "user namespaces look disabled: Apptainer builds may need fakeroot or --backend squashfuse"
fi
cat > "$CONF" <<EOF
# Written by installation.sh on $(hostname) at $(date). Machine-local; do not commit.
# Precedence: CVENV_* env vars > ~/.config/cvenv/config.json (cvenv config set) > this file.
export CVENV_INSTALL_PIXI=$(printf '%q' "$PIXI")
export CVENV_INSTALL_APPTAINER=$(printf '%q' "$APPT")
export CVENV_INSTALL_APPTAINER_CONF=$(printf '%q' "$APPT_CONF")
export CVENV_INSTALL_SQUASHFUSE=$(printf '%q' "$SQFS")
export CVENV_INSTALL_BUILD_TMPDIR=$(printf '%q' "$BUILD_TMP")
export CVENV_INSTALL_REPO=$(printf '%q' "$REPO_SLUG")
EOF
cfg() {  # cfg set|unset KEY [VALUE] - quiet unless it fails
    local out
    out="$("$CVENV" config "$@" 2>&1)" || fail "cvenv config $*: $out"
}
cfg set store "$STORE"
mkdir -p "$STORE"
cfg set rebuild-mode "$MODE"
if [ -n "$SLURM_ARGS" ]; then cfg set slurm-args "$SLURM_ARGS"; else cfg unset slurm-args; fi
# settings newer than the installed binary (e.g. an older release) are skipped
supports() { "$CVENV" config get "$1" >/dev/null 2>&1; }
if supports backend; then
    if [ "$BACKEND" = auto ]; then cfg unset backend; else cfg set backend "$BACKEND"; fi
elif [ "$BACKEND" != auto ]; then
    warn "$("$CVENV" --version) has no 'backend' setting; --backend $BACKEND was not saved"
fi
if [ "$USES" = squashfuse ] && ! supports squashfuse; then
    warn "$("$CVENV" --version) predates the squashfuse backend, so it can't build envs here."
    warn "Install a newer release, or build from a checkout: ./installation.sh --from-source"
fi
# an explicit build-tmpdir in the user config would shadow install.conf: keep them in sync
if [ -n "$(cfg_get build-tmpdir)" ]; then cfg set build-tmpdir "$BUILD_TMP"; fi

BLOCK_START="# >>> cvenv >>>"
BLOCK_END="# <<< cvenv <<<"
block="$BLOCK_START
export PATH=\"$PREFIX/bin:\$PATH\"
eval \"\$(\"$CVENV\" shell-init)\"
$BLOCK_END"
if [ "$MODIFY_RC" = yes ]; then
    step "Setting up $(tildify "$RCFILE")"
    touch "$RCFILE"
    if grep -qF "$BLOCK_START" "$RCFILE"; then
        # replace the existing block (the install may have moved)
        tmp="$(mktemp)"
        awk -v s="$BLOCK_START" -v e="$BLOCK_END" '$0==s{skip=1} !skip{print} $0==e{skip=0}' "$RCFILE" > "$tmp"
        printf '%s\n' "$block" >> "$tmp"
        cat "$tmp" > "$RCFILE"; rm -f "$tmp"
    else
        printf '\n%s\n' "$block" >> "$RCFILE"
    fi
fi

# ---------------------------------------------------------------- done
printf '\n%s%s is installed.%s\n\n' "$B" "$("$CVENV" --version)" "$R" >&2
if [ "$MODIFY_RC" = yes ]; then
    printf '  Open a new shell (or run: source %s), then:\n' "$(tildify "$RCFILE")" >&2
else
    printf '  Add this to your shell rc to use cvenv:\n\n%s\n\n  Then:\n' "$(printf '%s\n' "$block" | sed 's/^/    /')" >&2
fi
cat >&2 <<'EOF'
    cvenv init myenv python=3.12 numpy
    cvenv rebuild --env myenv
    cvenv run --env myenv python

  Settings: cvenv config    Help: cvenv --help    Update: cvenv self-update
EOF
