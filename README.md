# cvenv

Pixi environments packed into a single file, for clusters with file-count quotas.

A conda or pixi environment typically consists of 10,000–200,000 files. cvenv keeps each env as a
normal pixi workspace (`pixi.toml` + `pixi.lock`) and installs it into one image file instead.
Commands run from the image with your environment variables, working directory and GPUs passed
through. Images are built either with [Apptainer](https://apptainer.org) or, where Apptainer is
unavailable, as SquashFS images mounted without root ([backends](docs/backends.md)).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/amarchakitus/cvenv/main/installation.sh | bash
```

or from a checkout: `git clone https://github.com/amarchakitus/cvenv && cd cvenv && ./installation.sh`.

Requires Linux with Apptainer, or with FUSE for the squashfuse backend. The installer detects
what's available, asks where envs should be stored (plus sbatch arguments on Slurm clusters and
whether to set up your shell), and installs any missing tools into your home directory.

For a non-interactive install:

```bash
./installation.sh --yes --store /project/me/envs --slurm-args "-A myacct -p mypart" --modify-rc
```

Re-run the installer to upgrade; your settings are kept. See `./installation.sh --help` for all
options.

## Quick start

```bash
cvenv init myenv python=3.12 numpy        # creates <store>/myenv/pixi.toml
cvenv activate myenv
cvenv add scipy "pandas>=2"               # edits pixi.toml only
cvenv add --pypi wandb
cvenv rebuild                             # builds the image
python script.py                          # runs from the image
cvenv deactivate
```

For a GPU env, declare the CUDA version: `cvenv init gpuenv --cuda 12.0 python=3.12 pytorch-gpu`.

## Commands

| Command | Description |
|---|---|
| `cvenv init NAME [PKG…]` | Creates an env in `<store>/NAME` (`--path DIR`, `--cuda VER`, `-c CHANNEL`) |
| `cvenv init NAME --import FILE` | Creates an env from `environment.yml` or `requirements.txt` |
| `cvenv init NAME --manifest PATH` | Registers an existing `pixi.toml` or `pyproject.toml` |
| `cvenv wrap NAME IMAGE` | Uses an existing container: `.sif`, `docker://…`, `oras://…` |
| `cvenv wrap-install NAME DIR` | Packs an existing installation, such as a conda env, into one file |
| `cvenv activate [NAME]`, `cvenv deactivate` | Puts the env's commands on `PATH`, or remove them |
| `cvenv add …`, `cvenv remove …` | Changes dependencies (pixi syntax); takes effect on rebuild |
| `cvenv rebuild` | Builds the image (`--slurm`, `--incremental`, `--update`, `--backend`) |
| `cvenv run CMD…` | Runs a command or pixi task in the env |
| `cvenv shell` | Starts an interactive shell in the env |
| `cvenv envs` | Lists envs and their status |
| `cvenv info`, `cvenv list [PATTERN]` | Shows env details, or installed packages |
| `cvenv task …` | Manages pixi tasks |
| `cvenv move NAME DEST` | Moves an env to another directory |
| `cvenv share NAME`, `cvenv register PATH` | Shares an env with your group, or add a shared one |
| `cvenv unregister NAME [--delete]` | Forgets an env, optionally deleting its files |
| `cvenv config [get\|set\|unset KEY [VALUE]]` | Shows or changes settings |

Commands act on the env given by `--env NAME` (or `--env /path/to/env`), otherwise on the active
env or the env whose directory you are in. `cvenv agent-guide` prints a usage guide for AI
coding agents.

## Common tasks

**Build large envs on a compute node**

```bash
cvenv config set slurm-args "-A myacct -p mypart"   # once
cvenv rebuild --slurm --wait
cvenv config set rebuild-mode slurm                 # optional: always build with Slurm
```

**Rebuild faster after a small change**

```bash
cvenv rebuild --incremental
```

**Use an env in a batch job**

```bash
#!/bin/bash
#SBATCH --gres=gpu:1
eval "$(cvenv activate myenv)"
srun python train.py
```

Alternatively, call a command directly through its shim: `<env>/.cvenv/bin/python train.py`.

**Use an existing container**

```bash
cvenv wrap ngc docker://nvcr.io/nvidia/pytorch:24.10-py3
cvenv run --env ngc python train.py
```

**Shrink an existing conda env**

```bash
cvenv wrap-install oldenv /project/me/envs/oldenv --replace
cvenv run --env oldenv python -c "import numpy"   # check that it works
rm -rf /project/me/envs/oldenv.cvenv-orig-*      # then delete the original files
```

The original path (`/project/me/envs/oldenv/bin/python`) keeps working.

**Share an env with your group**

```bash
cvenv share myenv                     # as the owner
cvenv register /project/me/envs/myenv # as a group member, once
```

## Good to know

- Don't `pip install` into an env; use `cvenv add --pypi PKG` and `cvenv rebuild`. cvenv warns
  when an env's manifest has changed since its last build.
- With the squashfuse backend, mounts belong to one user on one node, so two users can't use a
  shared env on the same node at the same time. See [backends](docs/backends.md) for details.
- Settings, per-env options and environment variables are documented in
  [configuration](docs/configuration.md).

## Development

```bash
./installation.sh --from-source   # build bin/cvenv with pixi and install it
pixi run build                    # rebuild bin/cvenv
pixi run test
```

Pushing a `v*` tag publishes static x86_64 and aarch64 binaries through
`.github/workflows/release.yml`.
