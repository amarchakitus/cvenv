# cvenv — guide for AI agents

`cvenv` runs **pixi environments packed into single-file images** (and can also wrap
existing containers and existing installations). It exists because the
filesystem has a strict **file-count quota**: a normal conda/pixi/venv env (10k–200k files) must
not be created on it. Each cvenv env is a plain pixi workspace (`pixi.toml` + `pixi.lock`); the
installed environment lives in one image file under `<env>/.cvenv/`. Two backends (`cvenv envs`
BACKEND column, `cvenv info`):

- **apptainer** (`image.sif`): commands run inside a container with the host env vars, cwd,
  `$HOME`, `/tmp`, site paths (e.g. `/project`, `/scratch`) and GPUs passed through.
- **squashfuse** (`image-<build>.sqfs`): the image is mounted (no root) at
  `<mount-root>/<env-id>/<build>/` (default mount root `<store>/.cvenv-mnt/`) on first use on a
  node; commands run directly on the host from there.

You use both the same way (`cvenv run`, shims, `activate`).

Print this guide any time: `cvenv agent-guide`.

## Rules (read first)

1. **Never create Python/conda envs on the shared filesystem** — no `python -m venv`, `conda create`,
   `pixi install`, `pip install --user`, `uv venv`. Use a cvenv env instead.
2. **Never `pip install` into a cvenv env.** The image is read-only. Add dependencies to the
   manifest and rebuild: `cvenv add [--pypi] PKG --env NAME && cvenv rebuild --env NAME`.
3. **Always pass `--env NAME`.** Your shell commands usually run in fresh non-interactive shells:
   `cvenv activate` does not persist between them, and the `cvenv` shell function from `~/.bashrc`
   is not loaded. Use `cvenv run --env NAME CMD...` or the shim path `<env-path>/.cvenv/bin/CMD`.
4. **Do not build heavy envs on a login node** (PyTorch/CUDA, TensorFlow, large geo stacks). Use
   `cvenv rebuild --env NAME --slurm --wait`. Small envs (a few packages) are fine locally.
5. **Rebuild after every manifest change** (`add`, `remove`, hand edits to `pixi.toml`). Until then
   the image still contains the old packages and cvenv prints a staleness warning.
6. Batch dependency changes: several `cvenv add` calls, then **one** rebuild.
7. Don't edit or delete files under `<env>/.cvenv/` by hand (except reading logs).
8. **Ask the user before** `cvenv wrap-install ... --replace`, `cvenv share`, `cvenv unregister --delete`,
   or deleting any `*.cvenv-orig-*` backup: these change or remove the user's files or permissions.

## Decide what to do

| Situation | Do this |
|---|---|
| Need to run Python/a tool for this project | `cvenv envs` → pick env → `cvenv run --env NAME python script.py` |
| No suitable env exists | `cvenv init NAME python=3.12 pkg1 pkg2` → `cvenv rebuild --env NAME [--slurm --wait]` |
| `ModuleNotFoundError` / missing tool | `cvenv add --env NAME pkg` (conda-forge name) or `cvenv add --env NAME --pypi pkg` → rebuild |
| Env status `stale` | `cvenv rebuild --env NAME` (manifest/lock changed since build) |
| Env status `not built` | `cvenv rebuild --env NAME` |
| Env status `moved` | works as-is; rebuild only if pixi tasks or editable/path deps are used |
| Status `queued (job N)` / `building (job N)` | a Slurm build is in progress; wait, check `<env>/.cvenv/build-N.log` |
| Need a GPU | run inside a GPU Slurm allocation; `--nv` is added automatically when `/dev/nvidia*` exists |
| Project already has a `pixi.toml`/`pyproject.toml` | `cvenv init NAME --manifest /path/to/project` (registers it; image goes to `<store>/NAME/.cvenv/`) |
| Project has an `environment.yml` / `requirements.txt` | `cvenv init NAME --import environment.yml` (or `--import requirements.txt --python 3.12`) → rebuild |
| Need a prebuilt container (NGC, vendor image, `.sif`) | `cvenv wrap NAME docker://nvcr.io/nvidia/pytorch:24.10-py3` (or a `.sif` path, `--link` to not copy) |
| An existing big env/software tree eats the file quota | (ask first) `cvenv wrap-install NAME /path/to/env --replace` → verify → user deletes `/path/to/env.cvenv-orig-*` |
| Someone shared an env directory | `cvenv run --env /path/to/env CMD` (no setup) or `cvenv register /path/to/env` once |
| Small dependency change to a big env | `cvenv rebuild --env NAME --incremental` (reuses the current image; auto falls back to a full build) |
| Apptainer missing or broken on this cluster | builds use squashfuse automatically when Apptainer isn't installed; otherwise `cvenv rebuild --env NAME --backend squashfuse` (ask before changing the global `backend` setting) |

Prefer conda-forge packages (`cvenv add numpy`); use `--pypi` only for packages not on conda-forge.
GPU builds of PyTorch etc. need a CUDA system requirement in the manifest: create the env with
`cvenv init NAME --cuda 12.0 ...` (adds `[system-requirements] cuda = "12.0"`).

## Recipes

```bash
# discover
cvenv envs                                   # NAME, STATUS, SIZE, PATH (stdout)
cvenv info --env NAME                        # paths, build time, GPU flags, staleness reasons
cvenv list --env NAME numpy                  # installed packages (substring filter), from build time

# create + build
cvenv init NAME python=3.12 numpy pandas     # writes <store>/NAME/pixi.toml only; no build
cvenv init NAME --path /some/dir python=3.12 # custom location
cvenv rebuild --env NAME                     # local build (~1 min for small envs)
cvenv rebuild --env NAME --slurm --wait      # build as a Slurm job, block until done (exit code = job result)
cvenv rebuild --env NAME --incremental       # only install what changed (combinable with --slurm)
cvenv init NAME --import environment.yml     # or requirements.txt [--python 3.12]

# other env kinds
cvenv wrap NAME docker://python:3.12-slim    # existing container; settings in <env>/cvenv.toml
cvenv wrap-install NAME /path/to/conda-env   # squash an existing install; --replace leaves a stub
cvenv run --env /path/to/env python x.py     # any env by directory (also shared ones)

# change dependencies (manifest only), then rebuild
cvenv add --env NAME "scipy>=1.11" xarray
cvenv add --env NAME --pypi some-pypi-only-pkg
cvenv remove --env NAME xarray
cvenv rebuild --env NAME

# run
cvenv run --env NAME python train.py --lr 1e-3     # command inside the env; exit code propagates
cvenv run --env NAME bash -c 'python a.py && python b.py'   # several commands, one container start
cvenv run --env NAME mytask                         # pixi task defined in pixi.toml
cvenv exec --env NAME CMD...                        # like run, never treats CMD as a task
/path/to/env/.cvenv/bin/python script.py            # shim: same as `cvenv run --env NAME python`

# batch job (non-interactive shell: eval the activation, or use run/shims)
#   #SBATCH ...
#   eval "$(cvenv activate NAME)"    # puts shims on PATH for the rest of the script
#   srun python train.py
```

Get an env's path for shim use: `cvenv info --env NAME | sed -n 's/^path *: //p'`.

## Behaviour you can rely on

- **Exit codes:** `run`/`exec`/shims return the command's exit code. cvenv's own errors exit `1`
  (messages on stderr prefixed `cvenv: error:`). `cvenv activate` run directly in a terminal exits `2`.
- **Streams:** data/listings (`envs`, `info`, `list`, `config`) → stdout; status, warnings
  (`cvenv: warning:`) and build logs → stderr. `cvenv activate`/`deactivate` print shell code on
  stdout meant for `eval`.
- **Inside the container:** `python`, `pip`, etc. resolve to the env. `sys.executable` is
  `<env>/.cvenv/bin/python`, valid on the host too (it is the shim there), so tools that re-launch
  the interpreter elsewhere (dask-jobqueue, ipykernel install, multiprocessing spawn) work. Console
  scripts (`jupyter`, …) still record the image-internal path; prefer `python -m MODULE` when that matters.
  Envs built by older cvenv versions need one rebuild for this (`cvenv info` shows "host paths"). Host env vars (incl. `SLURM_*`,
  `CUDA_VISIBLE_DEVICES`, `OMP_NUM_THREADS`) pass through; `PYTHONHOME`, `VIRTUAL_ENV` and host
  `CONDA_*` are dropped; `PYTHONNOUSERSITE=1` (no `~/.local` packages).
- **Visible paths:** `$HOME`, `/tmp`, the cwd, the env's manifest dir and the site bind paths
  (typically `/project`, `/scratch`). Others need `CVENV_BINDS=/data,/other:/mnt` or
  `binds = [...]` under `[tool.cvenv]` in the manifest.
- **multiprocessing / threads / `/dev/shm` / DataLoader workers:** work normally inside one
  command. Each *command start* costs ~0.4–0.6 s (container launch) — avoid host-side loops that
  start `python` thousands of times; loop inside Python or `cvenv run --env NAME bash script.sh`.
- **Host-only tools are not available inside the container** (e.g. `sbatch`, `squeue`, `module`).
  Call them from the host shell, not from Python running in the env. (squashfuse envs run on the
  host, so these do work there.)
- **squashfuse envs:** command start ~20 ms once mounted. All host paths are visible (no binds).
  `sys.executable` is the real interpreter under the mount point; it only exists on nodes where
  the env is mounted, so to start a process on another node use the shim path
  `<env>/.cvenv/bin/python` (it mounts first). Mounts unmount themselves when unused. Never
  delete or rename anything under the mount root (`<store>/.cvenv-mnt/`) by hand: cvenv removes
  old mount points itself, after a retention period.
- **Env kinds** (`cvenv envs` KIND column): `pixi` (manifest-based; add/remove/rebuild), `wrap`
  (existing image; `rebuild` re-pulls it), `wrap-install` (squashed existing tree mounted at its
  original path; `add`/`remove` don't apply). `cvenv list --env NAME` shows packages (pixi) or
  exposed commands (wrapped).
- **Shared envs:** anyone who can read the env directory can use it (`--env PATH`, shims, `register`);
  only the owner can rebuild. `cvenv info` shows "(not in your registry)" for such envs.
- **Staleness check:** compares dependency-relevant manifest content + `pixi.lock` with the image.
  Editing tasks/description does not make an env stale. `CVENV_NO_STALE_WARNING=1` silences it.
- **Rebuilds are atomic:** running jobs keep using the old image; failed builds leave the old image intact.

## Files

```
<env>/pixi.toml            # edit via cvenv add/remove or by hand (plain pixi manifest)
<env>/pixi.lock            # written by builds; describes exactly what is in the image
<env>/.cvenv/image.sif     # the environment, apptainer backend (do not modify)
<env>/.cvenv/image-*.sqfs  # the environment, squashfuse backend
<store>/.cvenv-mnt/        # squashfuse mount points (empty directories; managed by cvenv)
<env>/.cvenv/bin/          # shims (python, ...): symlinks to the cvenv binary
<env>/.cvenv/build.log     # last local build log; build-<jobid>.log for Slurm builds
<env>/.cvenv/packages.txt  # `pixi list` of the image
~/.config/cvenv/config.json   # settings (cvenv config); registry.json: env name -> path
```

Optional per-env settings live in the manifest under `[tool.cvenv]`: `base-image`,
`post-commands` (both require rebuild), `gpu = "auto"|true|false|"rocm"`, `binds`, `env = {K = "V"}`,
`apptainer-args`, `shim-exclude`, `shim-include`, `isolate-user-site` (`gpu`, `binds`,
`apptainer-args` only matter for the apptainer backend).

## Settings

`cvenv config` lists them with their source. Relevant keys: `store` (where `init` creates envs),
`rebuild-mode` (`local`|`slurm` default for `rebuild`), `slurm-args` (sbatch args, e.g. account and
partition), `build-tmpdir` (node-local scratch for builds), `backend` (`auto`|`apptainer`|`squashfuse`),
`mount-timeout`, `mount-mode`, `mount-root`, `mount-retention` (squashfuse mounts). Change with
`cvenv config set KEY VALUE`. Ask the user before changing global settings.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `no env selected` | pass `--env NAME` |
| `unknown env 'X'` | `cvenv envs` for names; `cvenv init` to create; a shared env works by path: `--env /path` |
| `needs a pixi env, but 'X' is a wrap env` | wrapped envs have no manifest; change the source image (`cvenv wrap`) instead |
| `only its owner can rebuild it` | shared env: ask its owner, or make your own copy (`cvenv init NAME --manifest /path/to/env/pixi.toml`) |
| `incremental build not possible (...)` | informational; a full build runs instead |
| `has not been built yet` | `cvenv rebuild --env NAME` |
| build fails | read `<env>/.cvenv/build.log` (or `build-<jobid>.log`); usual causes: bad package name/version spec, solver conflict, no network on the build node |
| `Virtual package '__cuda' does not match` | env needs `[system-requirements] cuda = "12.x"` (`cvenv init --cuda`) |
| `sbatch failed` / account errors | `cvenv config get slurm-args`; pass `--slurm-args "-A ACCOUNT -p PARTITION"` |
| `pip install` refused | expected; use `cvenv add --pypi PKG` + rebuild |
| file not found inside container | path not bound: `CVENV_BINDS=/that/path cvenv run --env NAME ...` |
| `apptainer not found` | `cvenv config set apptainer /path/to/apptainer` (or load the site module) |
| debug the container invocation | `CVENV_DEBUG=1 cvenv run --env NAME ...` prints the apptainer command (or the squashfuse mount + exec) |
| `squashfuse not found` | `cvenv config set squashfuse /path/to/squashfuse_ll`, or re-run installation.sh |
| `is mounted by another user on this node` | a shared squashfuse env is in use by someone else on this node; use another node or wait |
| `Transport endpoint is not connected` | a stale squashfuse mount; cvenv repairs it on the next command. If a running process hit it, rerun it |
