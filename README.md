# cvenv

Pixi environments stored as a single image file, for clusters with file-count quotas.
Each env is a normal pixi workspace (`pixi.toml` + `pixi.lock`); `cvenv` builds it into one
file under `<env>/.cvenv/` and runs commands from it, with your env vars, working directory
and GPUs passed through. Two backends:

- **apptainer**: an Apptainer image (`image.sif`); commands run inside a container.
- **squashfuse**: a SquashFS image, mounted without root; commands run directly on the host.
  For clusters without (working) Apptainer.

`backend = auto` (the default) uses Apptainer when it is installed and squashfuse otherwise.

## Install

Requires Linux and either Apptainer (or Singularity), or FUSE (`/dev/fuse`, `fusermount3`) for
the squashfuse backend. `installation.sh` finds squashfuse, or fetches it without root on
Debian/Ubuntu (`apt-get download`) when neither backend is installed.

```bash
git clone https://github.com/amarchakitus/cvenv && cd cvenv
./installation.sh              # guided setup
```

Without a checkout:

```bash
curl -fsSL https://raw.githubusercontent.com/amarchakitus/cvenv/main/installation.sh | bash
```

Scripted install (no prompts):

```bash
./installation.sh --headless --store /project/me/containerized_envs \
    --rebuild-mode slurm --slurm-args "-A myacct -p mypart" --modify-rc
```

See `./installation.sh --help` for all options. Re-run it to upgrade or reconfigure.

## Quick start

```bash
cvenv init myenv python=3.12 numpy        # creates <store>/myenv/pixi.toml
cvenv activate myenv
cvenv add scipy "pandas>=2"               # edits pixi.toml only
cvenv add --pypi wandb
cvenv rebuild                             # builds the image
python script.py                          # runs inside the container
cvenv deactivate
```

GPU env: `cvenv init gpuenv --cuda 12.0 python=3.12 pytorch-gpu`.

## Commands

| Command | Description |
|---|---|
| `cvenv init NAME [PKG…]` | new env in `<store>/NAME` (`--path DIR`, `--cuda VER`, `-c CHANNEL`) |
| `cvenv init NAME --import FILE` | from `environment.yml` or `requirements.txt` (`--python VER`) |
| `cvenv init NAME --manifest PATH` | register an existing `pixi.toml`/`pyproject.toml` |
| `cvenv wrap NAME IMAGE` | use an existing container: `.sif`, `docker://…`, `oras://…` (`--link`) |
| `cvenv wrap-install NAME DIR` | squash an existing install (e.g. a conda env) into one file (`--replace`) |
| `cvenv activate [NAME]` / `cvenv deactivate` | put the env's commands on PATH |
| `cvenv add …` / `cvenv remove …` | change dependencies (pixi syntax, e.g. `--pypi`); no build |
| `cvenv rebuild` | build the image (`--slurm`, `--wait`, `--incremental`, `--update`, `--local`, `--backend`) |
| `cvenv run CMD…` | run a command or pixi task in the env |
| `cvenv shell` | interactive shell in the container |
| `cvenv envs` | list envs and their status |
| `cvenv info` / `cvenv list [PATTERN]` | env details / installed packages |
| `cvenv task …` | manage pixi tasks |
| `cvenv move NAME DEST` | relocate an env |
| `cvenv share NAME` / `cvenv register PATH` | share an env (`--world`, `--group`) / add a shared one |
| `cvenv unregister NAME [--delete]` | forget (and optionally delete) an env |
| `cvenv config [get\|set\|unset KEY [VALUE]]` | settings |
| `cvenv agent-guide` | usage guide for AI agents |

Select an env with `--env NAME` or `--env /path/to/env`. Otherwise the active env is used, or
the env whose directory you are in.

## Common tasks

**Build big envs on a compute node**

```bash
cvenv config set slurm-args "-A myacct -p mypart"   # once
cvenv rebuild --slurm --wait
cvenv config set rebuild-mode slurm                 # optional: make Slurm the default
```

**Rebuild faster after a small change**

```bash
cvenv rebuild --incremental      # reuses the current image; falls back to a full build if needed
```

**Batch jobs**

```bash
#!/bin/bash
#SBATCH --gres=gpu:1
eval "$(cvenv activate myenv)"
srun python train.py
```

Or call a shim directly: `<env>/.cvenv/bin/python train.py`.

**Use an existing container**

```bash
cvenv wrap ngc docker://nvcr.io/nvidia/pytorch:24.10-py3
cvenv run --env ngc python train.py
```

Runtime settings for wrapped envs go in `<env>/cvenv.toml`. `cvenv rebuild` re-pulls the image.

**Shrink an existing conda env**

```bash
cvenv wrap-install oldenv /project/me/envs/oldenv --replace
cvenv run --env oldenv python -c "import numpy"   # check that it works
rm -rf /project/me/envs/oldenv.cvenv-orig-*      # then delete the original files
```

The old path (`/project/me/envs/oldenv/bin/python`) keeps working.

**Share with your group**

```bash
cvenv share myenv                                  # owner
cvenv register /path/to/containerized_envs/myenv   # others, once; or use --env /path/...
```

## Backends

|   | apptainer | squashfuse |
|---|---|---|
| image | `.cvenv/image.sif` | `.cvenv/image-<build>.sqfs`, mounted at `<mount-root>/<env-id>/<build>/` |
| needs | Apptainer | FUSE + squashfuse (no root, no user namespaces) |
| commands run | in a container (OS from the base image) | on the host (host OS, all host paths) |
| host commands (`sbatch`, …) | not available | available |
| start-up cost | ~0.5 s per command | ~20 ms (first use on a node: mount, <1 s) |
| GPUs | `--nv`/`--rocm` added automatically | host drivers used directly |
| `wrap` (existing containers) | yes | no |

Pick one per build with `cvenv rebuild --backend apptainer|squashfuse`, or set a default with
`cvenv config set backend ...`. An env keeps its backend on later rebuilds unless told otherwise.

How squashfuse envs work:

- The build installs through the mount point (temporarily a symlink to node-local scratch), so the
  env's files never touch the shared filesystem; the tree is then squashed into one file.
- Mount points live in `mount-root` (default `<store>/.cvenv-mnt/`), one empty directory per
  build, never inside the env directory: `cvenv move` doesn't touch them, so moved envs keep
  working without a rebuild. A rebuilt or deleted env's old mount point is kept for
  `mount-retention` days (default 14; jobs on other nodes may still use it), then removed if it
  is empty and not mounted on that node. Removing or renaming a directory that is mounted on
  another node would leave a dead mount entry there that only root can clear.
- Shims and `cvenv run` mount the image on demand, once per node. An unused mount unmounts itself
  after `mount-timeout` seconds (default 600); stale mounts (e.g. from killed jobs) are cleaned up.
- Under Slurm, a job's processes are killed when it ends, which would break other jobs on that node
  still using the mount. So if your systemd user instance runs on the node, cvenv mounts through
  `systemd-run --user` and the mount outlives the job (`mount-mode = auto`; `direct` turns this off).
- Mounts belong to one user: two users can't use a shared env on the same node at the same time.
- Don't copy env directories by hand (`cp -r`): a copy shares the original's mount area. Use
  `cvenv init NAME --manifest /path/to/pixi.toml` instead.
- `sys.executable` and other paths point into the mount point. They work on any node where the env is
  mounted; the shims (`.cvenv/bin/python`) mount it first, so use those when starting things elsewhere.

## Notes

- Don't `pip install` into an env: use `cvenv add --pypi PKG` and `cvenv rebuild`.
- After changing dependencies, cvenv warns until you rebuild.
- Apptainer: paths visible in the container are `$HOME`, `/tmp`, the current directory and the
  site's default binds (e.g. `/project`, `/scratch`). Add more with `CVENV_BINDS=/a,/b` or
  `binds = [...]`. Host commands (`sbatch`, `module`, …) aren't available inside the container.

## Settings

`cvenv config set KEY VALUE` (stored in `~/.config/cvenv/config.json`):

| Key | Meaning |
|---|---|
| `store` | where new envs are created |
| `backend` | `auto`, `apptainer` or `squashfuse` |
| `rebuild-mode` | `local` or `slurm` |
| `slurm-args` | sbatch arguments for Slurm builds |
| `build-tmpdir` | node-local scratch for builds (default `/tmp`) |
| `pixi-cache` | package cache for builds |
| `base-image` | default base image |
| `apptainer`, `squashfuse`, `pixi` | tool paths (set by the installer) |
| `mount-timeout` | seconds before an unused squashfuse mount is unmounted (default 600) |
| `mount-mode` | squashfuse mounts: `auto` (systemd user unit if available), `systemd` or `direct` |
| `mount-root` | where squashfuse images are mounted (default `<store>/.cvenv-mnt`) |
| `mount-retention` | days an old build's mount point is kept before removal (default 14) |

Per-env options go in `pixi.toml` under `[tool.cvenv]`, or in `cvenv.toml` for wrapped envs:

```toml
[tool.cvenv]
gpu = "auto"                  # auto | true | false | rocm
binds = ["/data", "/in:/mnt:ro"]
env = { OMP_NUM_THREADS = "8" }
base-image = "docker://..."   # needs a rebuild
post-commands = ["..."]       # needs a rebuild
apptainer-args = []
shim-exclude = []
shim-include = []
host-valid-paths = true
isolate-user-site = true
```

`gpu`, `binds` and `apptainer-args` apply to the apptainer backend only.

Environment variables: `CVENV_GPU`, `CVENV_BINDS`, `CVENV_APPTAINER_ARGS`, `CVENV_BACKEND`,
`CVENV_NO_STALE_WARNING`, `CVENV_NO_PROMPT`, `CVENV_DEBUG`, `CVENV_STORE`, `CVENV_CONFIG_DIR`.

## Development

```bash
./installation.sh --from-source   # build bin/cvenv with pixi, then set up
pixi run build                    # rebuild bin/cvenv
pixi run test
```

Releases: push a `v*` tag; `.github/workflows/release.yml` publishes static x86_64 and aarch64
binaries for `installation.sh` to download.
