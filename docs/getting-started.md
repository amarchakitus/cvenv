# Getting started

This tutorial takes about ten minutes and assumes you have used conda environments before.
It covers creating an environment, using it, and bringing existing conda environments over.

## Why cvenv

A conda environment is a directory tree with many files. Even a modest data-science env
(Python, NumPy, pandas, SciPy) has about 14,000 of them, which matters on clusters that
limit the number of files per user, and slows down shared filesystems.

cvenv keeps what you work with, the list of packages, as a plain text manifest, and installs
the environment into **a single image file**. Commands run from that image as if the env were
installed normally: your files, environment variables and GPUs are all available.

| | conda | cvenv |
|---|---|---|
| Packages defined in | the env itself (`environment.yml` exported by hand) | `pixi.toml`, with an exact lock file `pixi.lock` |
| Installed as | ~14,000 files | 1 image file (plus a few small helper files) |
| Changing packages | edits the installed env in place | edits the manifest; `cvenv rebuild` makes a new image |

cvenv uses [pixi](https://pixi.sh) for package management, so packages come from the same
conda-forge channel you already use, and PyPI packages are supported.

The examples assume cvenv is installed with shell integration (the installer offers to set it
up; see the [README](../README.md#install)).

## 1. Create an environment

In the directory of a project, run:

```bash
cd ~/projects/analysis
cvenv init . python=3.12 numpy
```

This creates `pixi.toml` in the project directory (and a git repository if it isn't in one
already) and registers an env named `analysis`, after the directory. The image will live in your
cvenv store (`~/.cvenv/envs/analysis` by default), keeping large files out of the project.

For an env that isn't tied to a project, give it a name instead, like `conda create -n`:

```bash
cvenv init myenv python=3.12 numpy
```

## 2. Add packages

```bash
cvenv add "pandas>=2" scipy      # conda packages (conda-forge)
cvenv add --pypi tqdm            # PyPI packages
```

Package specs use conda syntax. Nothing is installed yet; `add` only edits the manifest:

```toml
[dependencies]
python = "3.12.*"
numpy = "*"
pandas = ">=2"
scipy = "*"

[pypi-dependencies]
tqdm = "*"
```

Commit `pixi.toml` (and later `pixi.lock`) with your code: anyone can recreate the exact env from them.

## 3. Build the image

```bash
cvenv rebuild
```

```
cvenv: built 'analysis' in 11s: ~/.cvenv/envs/analysis/.cvenv/image-….sqfs (135.5M, 25 commands shimmed)
```

For large envs, build on a compute node instead of the login node:

```bash
cvenv rebuild --slurm --wait
```

## 4. Use it

Activate it like a conda env:

```bash
cvenv activate analysis
python -c "import numpy, pandas, scipy"
which python        # ~/.cvenv/envs/analysis/.cvenv/bin/python
cvenv deactivate
```

Or run single commands without activating:

```bash
cvenv run --env analysis python train.py
```

Every command in the env also has a fixed path (a "shim") that works from anywhere, which is
useful in batch scripts, Jupyter kernels and IDE settings:

```bash
~/.cvenv/envs/analysis/.cvenv/bin/python train.py
```

In a Slurm job (batch scripts may not load your shell setup, so use the `eval` form):

```bash
#!/bin/bash
#SBATCH --gres=gpu:1
eval "$(cvenv activate analysis)"
srun python train.py
```

GPUs are passed through automatically. For GPU packages, declare the CUDA version when creating
the env: `cvenv init gpuenv --cuda 12.0 python=3.12 pytorch-gpu`.

## 5. Change it

Edit dependencies at any time, then rebuild:

```bash
cvenv add matplotlib-base
cvenv rebuild
```

Until you rebuild, the env keeps working with the old image, and cvenv reminds you:

```
$ cvenv envs
  NAME      KIND          BACKEND     STATUS     SIZE  PATH
* analysis  pixi          squashfuse  stale    135.5M  ~/.cvenv/envs/analysis
```

Don't use `pip install` or `conda install` inside an env: the image is read-only, and the
manifest is what defines the env. Use `cvenv add` and `cvenv add --pypi`.

## 6. Bring existing conda environments

**From an `environment.yml`**: channels, conda packages and the `pip:` section are converted
into a `pixi.toml`:

```bash
cvenv init legacy --import environment.yml
cvenv rebuild
```

A `requirements.txt` works the same way (`--import requirements.txt --python 3.12`).

**An installed conda env, as-is**: `wrap-install` packs it into one file without
re-solving anything, and keeps its original path working:

```bash
cvenv wrap-install oldenv ~/miniconda3/envs/oldenv --replace
~/miniconda3/envs/oldenv/bin/python -c "import numpy"   # still works
rm -rf ~/miniconda3/envs/oldenv.cvenv-orig-*            # then free the files
```

In our test, a conda env with Python and NumPy went from 9,957 files to 3 at its original
path, plus a 97 MB image. Scripts, kernels and job files that point to the old path don't need to
change.

## conda to cvenv

| conda | cvenv |
|---|---|
| `conda create -n NAME python=3.12 numpy` | `cvenv init NAME python=3.12 numpy` then `cvenv rebuild` |
| `conda env create -f environment.yml` | `cvenv init NAME --import environment.yml` then `cvenv rebuild` |
| `conda activate NAME` / `conda deactivate` | `cvenv activate NAME` / `cvenv deactivate` |
| `conda install PKG` | `cvenv add PKG` then `cvenv rebuild` |
| `pip install PKG` | `cvenv add --pypi PKG` then `cvenv rebuild` |
| `conda remove PKG` | `cvenv remove PKG` then `cvenv rebuild` |
| `conda env list` | `cvenv envs` |
| `conda list [PATTERN]` | `cvenv list [PATTERN]` |
| `conda run -n NAME CMD` | `cvenv run --env NAME CMD` |
| `conda env remove -n NAME` | `cvenv unregister NAME --delete` |

## Next steps

- [README](../README.md): all commands, sharing envs with your group, prebuilt containers
  (`cvenv wrap docker://…`).
- [Configuration](configuration.md): the store location, Slurm defaults and other settings.
- [Backends](backends.md): how images are built and run (Apptainer or squashfuse).
