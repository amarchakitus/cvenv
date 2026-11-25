#!/usr/bin/env bash
# End-to-end test: drives a real cvenv binary through the getting-started workflow
# (init, add, rebuild, run/shims/activate, staleness, git, clone + rebuild, --import,
# wrap-install --replace, unregister). Run via `pixi run e2e [BINARY]`.
#
#   bash scripts/e2e.sh [CVENV_BINARY]     (default: $CVENV_E2E_BIN, else bin/cvenv)
#
# Everything happens in a fresh temporary directory: CVENV_CONFIG_DIR, CVENV_STORE, the
# build scratch and git's global config all point inside it, so your own settings and
# registry are never read or changed. Needs network access, pixi, git, and for the
# squashfuse backend squashfuse_ll + mksquashfs (for apptainer: apptainer).
#
# Environment:
#   CVENV_E2E_BACKEND  squashfuse (default) or apptainer
#   CVENV_E2E_KEEP=1   keep the temporary directory (printed at the end) for debugging
#   CVENV_PIXI, CVENV_SQUASHFUSE, CVENV_APPTAINER, CVENV_PIXI_CACHE   passed to cvenv as usual
set -eEuo pipefail   # -E: the ERR trap below also fires in functions and subshells

REPO="$(cd -P "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-${CVENV_E2E_BIN:-$REPO/bin/cvenv}}"
BACKEND="${CVENV_E2E_BACKEND:-squashfuse}"

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

CURRENT="setup"
step() {
    CURRENT="$*"
    printf '\n\033[1m==> %s\033[0m\n' "$*" >&2
}
fail() {
    printf '\n\033[1;31me2e: FAIL [%s]: %s\033[0m\n' "$CURRENT" "$*" >&2
    [ -z "${T:-}" ] || : >"$T/.failed"   # reported once, also from subshells
    exit 1
}
# a command that fails outside a check (set -e) still says where it happened
trap 'rc=$?; [ -e "${T:-/nonexistent}/.failed" ] && exit 1; fail "command failed (exit $rc): $BASH_COMMAND"' ERR

# Run cvenv, echoing the command (output goes to the caller); fail on a non-zero exit.
cv() {
    printf '+ cvenv %s\n' "$*" >&2
    "$BIN" "$@" || fail "\`cvenv $*\` exited with $?"
}
# Run cvenv and capture stdout into $OUT (stderr still shown); fail on a non-zero exit.
cv_out() {
    printf '+ cvenv %s\n' "$*" >&2
    OUT="$("$BIN" "$@")" || fail "\`cvenv $*\` exited with $?"
}

assert_file() { [ -f "$1" ] || fail "expected file $1"; }
assert_dir() { [ -d "$1" ] || fail "expected directory $1"; }
assert_missing() { [ ! -e "$1" ] && [ ! -L "$1" ] || fail "expected $1 not to exist"; }
# assert_contains HAYSTACK NEEDLE WHAT
assert_contains() {
    case "$1" in
        *"$2"*) ;;
        *) fail "$3: expected to find '$2' in:"$'\n'"$1" ;;
    esac
}
assert_eq() { [ "$1" = "$2" ] || fail "$3: expected '$2', got '$1'"; }

# The lines of a [section] in a TOML file (up to the next section header).
toml_section() {
    awk -v s="[$2]" '$0 == s {on = 1; next} /^\[/ {on = 0} on' "$1"
}

# Columns of `cvenv envs` for one env: KIND BACKEND STATUS (prints nothing if absent;
# STATUS is only the first word, e.g. "not" for "not built").
env_row() {
    "$BIN" envs | sed 's/^[ *] //' | awk -v n="$1" '$1 == n {print $2, $3, $4}'
}
# assert_status NAME STATUS [KIND]; built envs must use $BACKEND
assert_status() {
    local row kind backend status
    row="$(env_row "$1")"
    [ -n "$row" ] || fail "env '$1' is not listed by \`cvenv envs\`:"$'\n'"$("$BIN" envs)"
    read -r kind backend status <<<"$row"
    assert_eq "$status" "$2" "status of '$1' in \`cvenv envs\`"
    [ "$status" = not ] || assert_eq "$backend" "$BACKEND" "backend of '$1' in \`cvenv envs\`"
    [ -z "${3:-}" ] || assert_eq "$kind" "$3" "kind of '$1' in \`cvenv envs\`"
}
assert_registered() { [ -n "$(env_row "$1")" ] || fail "env '$1' is not registered"; }
assert_unregistered() { [ -z "$(env_row "$1")" ] || fail "env '$1' is still registered"; }

env_path() { "$BIN" info --env "$1" | sed -n 's/^path *: //p'; }

# Unmount every FUSE mount below $1 (squashfuse images mounted by the test).
unmount_below() {
    local fm mnt
    fm="$(command -v fusermount3 || command -v fusermount || true)"
    [ -n "$fm" ] && [ -r /proc/self/mountinfo ] || return 0
    # mountinfo field 5 is the mount point (octal-escaped); deepest first
    awk -v root="$1" '$5 == root || index($5, root "/") == 1 {print $5}' /proc/self/mountinfo | LC_ALL=C sort -r |
        while read -r mnt; do
            mnt="$(printf '%b' "$mnt")"
            "$fm" -u "$mnt" 2>/dev/null || "$fm" -uz "$mnt" 2>/dev/null || true
        done
}

# ---------------------------------------------------------------------------
# setup
# ---------------------------------------------------------------------------

[ -x "$BIN" ] || fail "cvenv binary not found or not executable: $BIN (run \`pixi run build\` or pass a path)"
BIN="$(cd -P "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
case "$BACKEND" in
    squashfuse | apptainer) ;;
    *) fail "CVENV_E2E_BACKEND must be squashfuse or apptainer, not '$BACKEND'" ;;
esac
command -v git >/dev/null || fail "git is required"
PIXI="${CVENV_PIXI:-$(command -v pixi || true)}"
[ -n "$PIXI" ] || PIXI="$HOME/.pixi/bin/pixi"
[ -x "$PIXI" ] || fail "pixi not found (put it on PATH or set CVENV_PIXI)"

T="$(mktemp -d "${TMPDIR:-/tmp}/cvenv-e2e.XXXXXX")"
T="$(cd -P "$T" && pwd)"
cleanup() {
    local rc=$?
    trap - ERR
    cd /
    unmount_below "$T"
    if [ "${CVENV_E2E_KEEP:-}" = 1 ]; then
        echo "e2e: kept $T" >&2
    else
        chmod -R u+w "$T" 2>/dev/null || true
        rm -rf "$T" || echo "e2e: could not remove $T completely" >&2
    fi
    exit "$rc"
}
trap cleanup EXIT

# isolate cvenv completely from the user's settings
for v in $(env | sed -n 's/^\(CVENV_[A-Z_]*\)=.*/\1/p'); do
    case "$v" in
        CVENV_PIXI | CVENV_SQUASHFUSE | CVENV_APPTAINER | CVENV_APPTAINER_CONF | CVENV_PIXI_CACHE \
            | CVENV_E2E_*) ;;
        *) unset "$v" ;;
    esac
done
export CVENV_CONFIG_DIR="$T/config"
STORE="$T/store"
export CVENV_STORE="$STORE"
export CVENV_BUILD_TMPDIR="$T/tmp"
export CVENV_BACKEND="$BACKEND"
export CVENV_PIXI="$PIXI"
export CVENV_NO_PROMPT=1
mkdir -p "$T/tmp" "$T/work"
# cvenv runs `git init`; the test commits. Don't depend on (or touch) the user's git config.
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL="$T/gitconfig"
git config --global user.name "cvenv e2e"
git config --global user.email "e2e"
git config --global init.defaultBranch main
# `pixi run e2e` activates cvenv's own build env: don't let its pixi/conda variables leak into
# the pixi that cvenv runs (PIXI_HOME and the cache locations are fine to keep)
for v in $(env | sed -n 's/^\(\(PIXI\|CONDA\)_[A-Z_]*\)=.*/\1/p'); do
    case "$v" in
        PIXI_HOME | PIXI_CACHE_DIR) ;;
        *) unset "$v" ;;
    esac
done
unset VIRTUAL_ENV

echo "e2e: testing $BIN ($("$BIN" --version)), backend $BACKEND, in $T" >&2
START=$SECONDS

# ---------------------------------------------------------------------------
step "init: a project directory"
# ---------------------------------------------------------------------------
PROJ="$T/work/analysis"
mkdir -p "$PROJ"
cd "$PROJ"
cv init . python=3.12
assert_file "$PROJ/pixi.toml"
assert_dir "$PROJ/.git"
assert_registered analysis
assert_status analysis "not"   # "not built"
assert_contains "$(toml_section pixi.toml dependencies)" "python" "pixi.toml [dependencies]"
STORE_ENV="$STORE/analysis"
assert_eq "$(env_path analysis)" "$STORE_ENV" "env path of a project env"
assert_missing "$PROJ/.cvenv"

step "init: a named env in the store gets no git repo"
cd "$T/work"
cv init scratch python=3.12
assert_file "$STORE/scratch/pixi.toml"
assert_missing "$STORE/scratch/.git"
assert_registered scratch

step "init: inside an existing repo, no nested repo"
mkdir -p "$PROJ/sub"
cd "$PROJ/sub"
cv init . --name nested
assert_file "$PROJ/sub/pixi.toml"
assert_missing "$PROJ/sub/.git"
assert_registered nested
cv unregister nested --delete --yes
assert_unregistered nested
assert_file "$PROJ/sub/pixi.toml"   # the project's files are never deleted
rm -rf "$PROJ/sub"

# ---------------------------------------------------------------------------
step "add (conda + PyPI) and rebuild"
# ---------------------------------------------------------------------------
cd "$PROJ"
cv add six
cv add --pypi iniconfig
assert_contains "$(toml_section pixi.toml dependencies)" "six" "pixi.toml [dependencies]"
assert_contains "$(toml_section pixi.toml pypi-dependencies)" "iniconfig" "pixi.toml [pypi-dependencies]"
cv rebuild
assert_status analysis ok pixi
assert_file "$PROJ/pixi.lock"

step "run, shims, activate"
cv_out run --env analysis python -c "import six, iniconfig, sys; print(sys.version_info[:2])"
assert_eq "$OUT" "(3, 12)" "python version via cvenv run"
SHIM="$STORE_ENV/.cvenv/bin/python"
[ -x "$SHIM" ] || fail "shim $SHIM is missing"
(cd / && "$SHIM" -c "import six, iniconfig") || fail "the shim $SHIM can't import the packages"
OUT="$(cd / && "$SHIM" -c 'import sys; print(sys.executable)')"
if [ "$BACKEND" = apptainer ]; then
    assert_eq "$OUT" "$SHIM" "sys.executable via the shim"   # the shim path, valid on the host
else
    case "$OUT" in   # the real interpreter under the mount point
        "$STORE/.cvenv-mnt/"*/bin/python) ;;
        *) fail "sys.executable via the shim: expected a path under $STORE/.cvenv-mnt, got $OUT" ;;
    esac
fi
"$BIN" run --env analysis python -c "import sys; sys.exit(7)" && rc=0 || rc=$?
assert_eq "$rc" 7 "exit code of \`cvenv run\`"
(
    eval "$("$BIN" activate analysis)"
    assert_eq "$(command -v python)" "$SHIM" "python on PATH after activate"
    python -c "import six, iniconfig" || fail "python after activate can't import the packages"
    assert_eq "${CVENV_ACTIVE:-}" analysis "CVENV_ACTIVE after activate"
)

step "list"
cv_out list --env analysis six
assert_contains "$OUT" "six" "cvenv list six"
cv_out list --env analysis iniconfig
assert_contains "$OUT" "iniconfig" "cvenv list iniconfig"

# ---------------------------------------------------------------------------
step "staleness: add makes the env stale, rebuild makes it ok"
# ---------------------------------------------------------------------------
cv add toolz
assert_status analysis stale
cv_out run --env analysis python -c "import six"   # old image still works
cv rebuild
assert_status analysis ok
cv run --env analysis python -c "import toolz"

# ---------------------------------------------------------------------------
step "git: .cvenv keeps itself out of git"
# ---------------------------------------------------------------------------
assert_gitignored() {
    assert_file "$1/.cvenv/.gitignore"
    grep -qx '\*' "$1/.cvenv/.gitignore" || fail "$1/.cvenv/.gitignore has no '*' line"
}
assert_clean_of_cvenv() {   # assert_clean_of_cvenv REPO_DIR: git status never shows .cvenv/
    local st
    st="$(git -C "$1" status --porcelain --untracked-files=all)"
    case "$st" in *.cvenv*) fail "git status in $1 shows .cvenv files:"$'\n'"$st" ;; esac
    assert_contains "$st" "pixi.toml" "git status in $1"
}
assert_gitignored "$STORE_ENV"
assert_missing "$PROJ/.cvenv"   # a project's images live in the store
assert_clean_of_cvenv "$PROJ"
assert_contains "$(git -C "$PROJ" status --porcelain)" "pixi.lock" "git status in $PROJ"
# an env whose images live next to its manifest, in a new repo (init NAME --path DIR)
cd "$T/work"
cv init tool --path "$T/work/tool"
assert_dir "$T/work/tool/.git"
assert_gitignored "$T/work/tool"
cv add --env tool python=3.12
cv rebuild --env tool
assert_status tool ok
assert_clean_of_cvenv "$T/work/tool"
cv unregister tool --delete --yes
assert_missing "$T/work/tool"

# ---------------------------------------------------------------------------
step "reproducibility: clone the project, rebuild with a fresh config and store"
# ---------------------------------------------------------------------------
git -C "$PROJ" add -A
git -C "$PROJ" commit -q -m "analysis env"
cv_out list --env analysis
PKGS1="$OUT"
mkdir -p "$T/work2"
git clone -q "$PROJ" "$T/work2/analysis"
(
    export CVENV_CONFIG_DIR="$T/config2" CVENV_STORE="$T/store2"
    cd "$T/work2/analysis"
    cv init .
    assert_eq "$(env_path analysis)" "$T/store2/analysis" "env path of the clone"
    cv rebuild
    assert_status analysis ok
    git diff --quiet HEAD -- pixi.lock pixi.toml ||
        fail "the rebuild in the clone changed pixi.lock/pixi.toml:"$'\n'"$(git diff HEAD | head -50)"
    cmp -s pixi.lock "$PROJ/pixi.lock" || fail "the clone's pixi.lock differs from the original"
    cv_out list --env analysis
    [ "$OUT" = "$PKGS1" ] ||
        fail "packages differ between original and clone:"$'\n'"$(diff <(echo "$PKGS1") <(echo "$OUT"))"
    "$T/store2/analysis/.cvenv/bin/python" -c "import six, iniconfig, toolz" ||
        fail "the clone's shim can't import the packages"
    st="$(git status --porcelain --untracked-files=all)"
    [ -z "$st" ] || fail "the clone's work tree is dirty after init + rebuild:"$'\n'"$st"
)

# ---------------------------------------------------------------------------
step "init --import environment.yml"
# ---------------------------------------------------------------------------
cd "$T/work"
cat >environment.yml <<'EOF'
name: legacy
channels:
  - conda-forge
dependencies:
  - python=3.12
  - six
  - pip
  - pip:
      - iniconfig==2.0.0
EOF
cv init legacy --import environment.yml
LEGACY="$STORE/legacy/pixi.toml"
assert_file "$LEGACY"
deps="$(toml_section "$LEGACY" dependencies)"
pypi="$(toml_section "$LEGACY" pypi-dependencies)"
grep -Eq '^python *= *"3\.12' <<<"$deps" || fail "[dependencies] lacks python 3.12:"$'\n'"$deps"
grep -Eq '^six *=' <<<"$deps" || fail "[dependencies] lacks six:"$'\n'"$deps"
grep -Eq '^iniconfig *= *"==2\.0\.0"' <<<"$pypi" ||
    fail "[pypi-dependencies] lacks iniconfig==2.0.0:"$'\n'"$pypi"
cv rebuild --env legacy
cv_out run --env legacy python -c "import six, importlib.metadata as m; print(m.version('iniconfig'))"
assert_eq "$OUT" "2.0.0" "iniconfig version in the imported env"

# ---------------------------------------------------------------------------
step "wrap-install --replace an existing conda env"
# ---------------------------------------------------------------------------
# a conda prefix made by pixi (much faster than conda create): <ws>/.pixi/envs/default
OLDWS="$T/work/oldproj"
mkdir -p "$OLDWS"
cat >"$OLDWS/pixi.toml" <<'EOF'
[workspace]
name = "oldproj"
channels = ["conda-forge"]
platforms = ["linux-64", "linux-aarch64"]

[dependencies]
python = "3.12.*"
six = "*"
EOF
cache="${CVENV_PIXI_CACHE:-$T/tmp/cvenv-$(id -un)/pixi-cache}"
PIXI_CACHE_DIR="$cache" RATTLER_CACHE_DIR="$cache" "$PIXI" install -q --manifest-path "$OLDWS/pixi.toml"
OLD="$OLDWS/.pixi/envs/default"
assert_dir "$OLD/conda-meta"
"$OLD/bin/python" -c "import six" || fail "the test conda env is broken before wrapping"
cv wrap-install oldenv "$OLD" --replace
assert_status oldenv ok wrap-install
# the original path now holds only the stub: a marker, and bin -> the env's shims
assert_stub() {
    local got
    got="$(cd "$OLD" && find . -mindepth 1 -maxdepth 1 | LC_ALL=C sort | tr '\n' ' ')"
    assert_eq "$got" "./.cvenv-wrapped ./bin " "contents of the replaced directory $OLD"
    [ -L "$OLD/bin" ] || fail "$OLD/bin is not a symlink to the shims"
}
assert_stub
backups=("$OLD".cvenv-orig-*)
[ -d "${backups[0]}" ] || fail "no backup $OLD.cvenv-orig-* of the original tree"
# old absolute paths keep working (scripts, kernels, shebangs)
OUT="$(cd / && "$OLD/bin/python" -c 'import six, sys; print(sys.prefix)')" ||
    fail "$OLD/bin/python no longer works after --replace"
assert_eq "$OUT" "$OLD" "sys.prefix via the original path"
cv_out run --env oldenv python -c "import six, sys; print(sys.prefix)"
assert_eq "$OUT" "$OLD" "sys.prefix via cvenv run"
# the original files can go: the env lives in its image now
rm -rf "${backups[@]}"
if [ "$BACKEND" = squashfuse ]; then
    unmount_below "$OLD"   # back to the bare stub: the next command mounts the image again
    assert_stub
fi
OUT="$(cd / && "$OLD/bin/python" -c 'import six; print("ok")')" ||
    fail "$OLD/bin/python stopped working once the backup was deleted"
assert_eq "$OUT" ok "original path after deleting the backup"

# ---------------------------------------------------------------------------
step "unregister --delete"
# ---------------------------------------------------------------------------
cv unregister scratch --delete --yes
assert_unregistered scratch
assert_missing "$STORE/scratch"
cv unregister legacy --delete --yes
assert_unregistered legacy
assert_missing "$STORE/legacy"
cv unregister analysis --delete --yes
assert_unregistered analysis
assert_missing "$STORE_ENV"
assert_file "$PROJ/pixi.toml"   # a project env's manifest is the user's: kept
assert_file "$PROJ/pixi.lock"
cv unregister oldenv --delete --yes
assert_unregistered oldenv
assert_missing "$STORE/oldenv"

trap - ERR
printf '\n\033[1;32me2e: all checks passed (%s backend, %ss)\033[0m\n' "$BACKEND" "$((SECONDS - START))" >&2
