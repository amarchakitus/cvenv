# cvenv — pixi environments in a single file

`cvenv` is a thin wrapper around [pixi](https://pixi.sh) for clusters with a file quota.
Each env is an **ordinary pixi workspace** (`pixi.toml` + `pixi.lock`). The difference is in
how it gets installed: `cvenv rebuild` installs the environment *inside an Apptainer image*,
so a CUDA/PyTorch stack that would normally be ~100k files becomes **one `.sif` file**.

Activating an env puts small *shims* (`python`, `jupyter`, `torchrun`, …) on your `PATH`. Each
shim runs the command inside the container, and passes through:

- the host environment (all env vars, including `SLURM_*` and `CUDA_VISIBLE_DEVICES`),
- your working directory,
- the usual bind mounts,
- GPUs, detected automatically.

```
$ cvenv init myenv --cuda 12.0 python=3.12 pytorch-gpu   # writes pixi.toml only
$ cvenv activate myenv
(myenv) $ cvenv add numpy "scipy>=1.11"                  # edits pixi.toml, no build
(myenv) $ cvenv add --pypi wandb
(myenv) $ cvenv rebuild --slurm --wait                   # builds .cvenv/image.sif on a compute node
(myenv) $ python train.py                                # runs inside the container
(myenv) $ srun python train.py                           # ... on every task, with GPUs
(myenv) $ cvenv deactivate
```

`cvenv` is a single static Rust binary (~3 MB) with no runtime dependencies besides pixi and Apptainer. Each shim is a
symlink to that binary, which works out from the name it was invoked under (`argv[0]`) which
command to run and in which env.

## Install

The prerequisites are Linux and Apptainer (or Singularity). cvenv itself is a prebuilt static
binary from the GitHub releases (x86_64 and aarch64), so no compiler is needed:

```bash
# from a checkout
git clone https://github.com/OWNER/cvenv && cd cvenv
./installation.sh

# or without a checkout (installs to ~/.cvenv/bin/cvenv by default)
curl -fsSL https://raw.githubusercontent.com/OWNER/cvenv/main/installation.sh | bash -s -- --repo OWNER/cvenv
```

By default the installer **walks you through the setup**. It first detects pixi, Apptainer
(including `module load apptainer`), Slurm and any existing cvenv settings, then asks about:

1. **install location**: the checkout, or `~/.cvenv`,
2. **where the binary comes from**: GitHub release download (default, verified against its
   `.sha256`), build from source, a local file, or keep the installed one,
3. **tools**: the pixi and apptainer executables (pixi can be installed for you),
4. **env store and build scratch directory**: where envs live, and the node-local disk for builds,
5. **Slurm builds** (only on Slurm clusters): the default rebuild mode and sbatch arguments such
   as `-A <account> -p <partition>`,
6. **shell integration**: whether to add the cvenv block to `~/.bashrc` (or another rc file).

Each question offers the detected or current value, and Enter keeps it. Nothing changes until
you confirm the summary, and you can choose *edit* to go through the questions again. Re-running
the installer later (to upgrade, reconfigure, or set up a new cluster) starts from your current
settings.

**Headless installs** (scripts, CI, provisioning) use `--headless`, or its aliases `--yes`/`-y`,
and never prompt. Flags decide, then current settings, then detected defaults. The shell rc is
only modified with `--modify-rc`. The installer also runs headless automatically when there is
no terminal.

```bash
./installation.sh --headless --store /project/me/containerized_envs \
    --rebuild-mode slurm --slurm-args "-A pi-me -p amd" --modify-rc
```

Every flag pre-answers the matching question, so flags are also handy in interactive mode:
`--prefix`, `--version`, `--repo`, `--url`, `--binary`, `--from-source`, `--keep-binary`,
`--store`, `--build-tmpdir`, `--rebuild-mode`, `--slurm-args`, `--modify-rc`/`--no-modify-rc`,
`--rc-file`, `--pixi`, `--apptainer` and `--no-pixi-install`. See `./installation.sh --help`.

What gets written:

- `<prefix>/bin/cvenv`,
- `<prefix>/etc/install.conf` (machine-local tool paths, git-ignored),
- the store, rebuild mode and sbatch arguments in `~/.config/cvenv/config.json`,
- optionally this block in your shell rc:

```bash
# >>> cvenv >>>
export PATH="/path/to/cvenv/bin:$PATH"
eval "$("/path/to/cvenv/bin/cvenv" shell-init)"
# <<< cvenv <<<
```

The `shell-init` line is what lets `cvenv activate` change your current shell, the same way
conda's hook does. Without it, use `eval "$(cvenv activate NAME)"` or
`source <env>/.cvenv/activate`.

## Commands

| command | what it does |
|---|---|
| `cvenv init NAME [--path DIR] [--cuda VER] [PKG…]` | create a pixi workspace in `<store>/NAME` (or `DIR`) and register it |
| `cvenv init NAME --manifest PATH` | register an existing `pixi.toml`/`pyproject.toml` (e.g. a project repo) |
| `cvenv envs` | list envs with status (`ok`, `stale`, `not built`, `moved`, `queued/building (job N)`) |
| `cvenv activate [NAME]` / `cvenv deactivate` | venv-style activation (name optional inside an env dir) |
| `cvenv add …` / `cvenv remove …` | `pixi add/remove --frozen` on the env's manifest; never builds |
| `cvenv rebuild [--update] [--no-cache]` | build the image; `--update` re-solves (`pixi update`) first |
| `cvenv rebuild --slurm [--wait] [--slurm-args "…"]` | build in a Slurm batch job instead (see below) |
| `cvenv run CMD…` / `cvenv exec CMD…` | run a command inside the env (`run` also accepts **pixi task** names) |
| `cvenv shell` | interactive shell inside the container |
| `cvenv list [PATTERN]` | packages in the image (`pixi list` captured at build time) |
| `cvenv task …` | `pixi task …` on the env's manifest |
| `cvenv info` | paths, build info, GPU flags, staleness |
| `cvenv move NAME DEST` | relocate an env directory (registry, shims and an active shell are updated) |
| `cvenv unregister NAME [--delete]` | forget an env (optionally delete its files) |
| `cvenv config [get\|set\|unset KEY [VALUE]]` | settings, see below |

Commands that work on "the env" use `--env NAME` (or `-n NAME`). Without it they fall back to
the active env, and then to the env whose directory you are in.

## Building on compute nodes

Big environments such as PyTorch + CUDA are too heavy to build on a login node. You can submit
the build as a batch job:

```bash
cvenv config set slurm-args "-A pi-myaccount -p amd"   # once
cvenv config set rebuild-mode slurm                    # optional: make --slurm the default
cvenv rebuild --slurm            # returns immediately; `cvenv envs` shows queued/building
cvenv rebuild --slurm --wait     # blocks until the job is done
cvenv rebuild --local            # force a local build when rebuild-mode = slurm
```

The job requests `--cpus-per-task=8 --mem=32G --time=02:00:00` by default. Your `slurm-args`
and then `--slurm-args` are added after these, so they can override them. The job runs
`cvenv rebuild --local` with your environment exported, builds in the node's `/tmp`, and logs
to `<env>/.cvenv/build-<jobid>.log`. Compute nodes need outbound internet to download packages.

## How it works

```
<store>/myenv/                 # a normal pixi workspace; commit it to git if you like
├── pixi.toml
├── pixi.lock                  # updated by the build: it describes what is in the image
├── .gitignore                 # contains .cvenv/
└── .cvenv/
    ├── image.sif              # the whole environment, one file
    ├── image.json             # build metadata + fingerprint
    ├── packages.txt           # pixi list of the image
    ├── build.log              # last build (build-<jobid>.log for Slurm builds)
    ├── activate               # `source` me
    └── bin/                   # shims: .cvenv-shim -> cvenv binary, python -> .cvenv-shim, ...
```

**Build.** `cvenv rebuild` writes an Apptainer definition file and runs an unprivileged
`apptainer build`. The base image is `ghcr.io/prefix-dev/pixi:<your pixi version>` by default;
your host pixi binary is copied in as well. During the build, the workspace directory is
bind-mounted at its real path, so editable/path dependencies resolve and `pixi.lock` is written
straight back to your workspace. pixi installs with `detached-environments` pointing inside the
image, so nothing is installed on the shared filesystem. The build's temporary files (the
unpacked base image, the rootfs and the package cache) all live in `build-tmpdir`, node-local
`/tmp` by default, and are deleted afterwards (the package cache is kept, for faster
rebuilds). The new image replaces the old one atomically, so jobs still running on the old
image are not disturbed.

**Activation inside the image.** At build time the output of `pixi shell-hook` (including
packages' `activate.d` scripts) is saved as a static script in `/.singularity.d/env/`. Every
`apptainer exec` therefore starts in a fully activated pixi environment.

**Runtime.** A shim runs
`apptainer exec [--nv|--rocm] [--bind …] image.sif <cmd> <args>`. Apptainer passes the host
environment through, except `PATH` and `LD_LIBRARY_PATH`, which belong to the container. cvenv
also:

- drops `PYTHONHOME`, `VIRTUAL_ENV` and host `CONDA_*`/`PIXI_*` activation variables,
- sets `PYTHONNOUSERSITE=1`, so `~/.local` packages don't leak in,
- sets `PIP_USER=0`.

`$HOME`, `/tmp`, the current directory and the site's bind paths (read from `apptainer.conf`,
e.g. `/project` and `/scratch`) are mounted. The workspace directory is added if it isn't
already covered.

**GPUs.** `--nv` is added automatically when `/dev/nvidia*` exists, which is the case on GPU
nodes inside a job; `--rocm` is added for `/dev/kfd`. Override with `CVENV_GPU=0|1|rocm` or
`gpu = …` in the manifest. Declaring `[system-requirements] cuda = "12.0"` (`cvenv init --cuda`)
makes pixi resolve CUDA builds even on a build node without a GPU; cvenv sets
`CONDA_OVERRIDE_CUDA` for the build to match.

**Staleness warnings.** The image records a fingerprint: a hash of the
dependency-relevant parts of the manifest plus a hash of `pixi.lock`. If they no longer match,
`activate`, `run`, `shell`, `list` and every shim print a warning telling you to run
`cvenv rebuild`. Editing tasks, the description and similar fields does not count as a change.
Moving an env produces a softer warning: the image still works, but pixi tasks and editable
dependencies reference the old path until you rebuild. Silence the warnings with
`CVENV_NO_STALE_WARNING=1`.

**pip.** The image is read-only, so `pip install` inside an active env is intercepted with a
hint to use `cvenv add --pypi PKG && cvenv rebuild` (`CVENV_ALLOW_PIP=1` bypasses this).
`pip`/`pip3` shims exist even if the env has no pip, so a host pip can never install into
`~/.local` by accident.

## Per-env options: `[tool.cvenv]` in the manifest

```toml
[tool.cvenv]
base-image = "docker://nvidia/cuda:12.4.1-base-ubuntu22.04"   # rebuild-relevant
post-commands = ["apt-get update && apt-get install -y git"]  # rebuild-relevant (run as fakeroot)
gpu = "auto"                    # auto | true | false | rocm
binds = ["/software", "/data/in:/in:ro"]
env = { OMP_NUM_THREADS = "8" } # extra variables (set with apptainer --env)
apptainer-args = ["--writable-tmpfs"]
isolate-user-site = true        # PYTHONNOUSERSITE=1
shim-exclude = ["sqlite3"]      # don't shim these (glob patterns)
shim-include = ["xz"]           # force-shim something excluded by default
```

pixi ignores the `[tool.cvenv]` table, so the manifest stays a valid pixi manifest.

## Global settings

`cvenv config set KEY VALUE` writes `~/.config/cvenv/config.json`. Env vars override the file,
and the file overrides `etc/install.conf`.

| key | env var | meaning |
|---|---|---|
| `store` | `CVENV_STORE` | where `cvenv init NAME` creates envs (default `~/.cvenv/envs`). Changing it doesn't move existing envs: use `cvenv move` |
| `rebuild-mode` | `CVENV_REBUILD_MODE` | `local` (default) or `slurm` |
| `slurm-args` | `CVENV_SLURM_ARGS` | sbatch arguments for Slurm builds, e.g. `-A pi-me -p amd` |
| `apptainer` | `CVENV_APPTAINER` | apptainer/singularity executable |
| `apptainer-conf` | `CVENV_APPTAINER_CONF` | used to detect the site bind paths |
| `pixi` | `CVENV_PIXI` | pixi executable |
| `build-tmpdir` | `CVENV_BUILD_TMPDIR` | node-local scratch for builds (default `/tmp`) |
| `pixi-cache` | `CVENV_PIXI_CACHE` | persistent package cache for builds (default `<build-tmpdir>/cvenv-$USER/pixi-cache`) |
| `base-image` | `CVENV_BASE_IMAGE` | default base image |

The registry (env name → directory) is `~/.config/cvenv/registry.json`. Point
`CVENV_CONFIG_DIR` elsewhere to keep a separate set of envs.

Other runtime variables: `CVENV_BINDS=a,b:c`, `CVENV_APPTAINER_ARGS="..."`, `CVENV_GPU`,
`CVENV_NO_PROMPT=1`, `CVENV_DEBUG=1` (prints the apptainer command).

## Batch jobs

```bash
#!/bin/bash
#SBATCH --partition=gpu --gres=gpu:1 --time=1:00:00
eval "$(cvenv activate myenv)"     # works without the shell hook
srun python train.py
```

Or call a shim directly, without activating: `<env>/.cvenv/bin/python train.py`. The same works
as an interpreter path for VS Code or Jupyter.

## Notes and limits

- Each command start costs about 0.3–0.6 s for the container launch. That is negligible for
  jobs, but noticeable if a shell script calls `python` thousands of times. Use one long-lived
  process, or `cvenv run bash script.sh`, for loops like that.
- Shims run *one* command in the container. A host shell script run while activated keeps using
  host tools, apart from the shimmed names. To run a whole script inside the container, use
  `cvenv run bash script.sh`.
- Commands that the host needs to stay fast (`clear`, `tput`, `xz`, …) are not shimmed by
  default; see `shim-exclude`/`shim-include`.
- Builds need unprivileged user namespaces (Apptainer ≥ 1.1 uses them automatically). The
  installer checks for them.

## Development

Build from source with pixi. The repo's `pixi.toml` provides the Rust toolchain and C compiler
from conda-forge:

```bash
./installation.sh --from-source   # first time: builds bin/cvenv, then the usual walkthrough
pixi run build                    # afterwards: rebuild bin/cvenv
pixi run test                     # unit tests
```

The toolchain's pixi env (`.pixi/config.toml` → `detached-environments`), the pixi package
cache and cargo's registry/target directories all live in node-local
`/tmp/cvenv-$USER/`, so a build adds no files to the quota'd filesystem. If you run `pixi run
build` without the installer, first run
`pixi config set --local detached-environments /tmp/cvenv-$USER/pixi-envs`.

`scripts/build.sh` links statically (`crt-static` against the conda glibc 2.28 sysroot), so the
binary runs on any Linux of that architecture. There are no musl targets on conda-forge.

**Releases.** Push a tag such as `v0.2.0`. `.github/workflows/release.yml` then builds the
x86_64 and aarch64 binaries with the same `pixi run build` and attaches them to the GitHub
release with `.sha256` files, which is where `installation.sh` downloads from. `ci.yml` runs
the tests and a build on every push to `main`.

Source layout: `src/main.rs` (CLI and commands), `src/env.rs` (registry, manifests,
fingerprints), `src/build.rs` (definition file, local and Slurm builds), `src/runtime.rs`
(apptainer command line, GPUs, binds, shims, shell activation), `src/config.rs`,
`src/util.rs`.
