# Changelog

All notable changes to cvenv are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] - 2026-11-25

### Added

- `cvenv init DIR` sets up a project directory, like `uv init .`. An argument that looks like a
  path (`.`, `..`, `~/proj`, anything containing `/`) creates `pixi.toml` there, or adopts an
  existing manifest. The env is named after the directory (`--name` to override) and its images
  go to `<store>/NAME` (or `--path DIR`), away from the project. Plain names behave as before.
- New project manifests outside the store get a git repository, unless the directory is already
  inside one or git is not installed. No initial commit is made; `--vcs git|none` overrides.

### Changed

- `.cvenv/` now contains its own `.gitignore` (with `*`), created by `init`, `wrap` and builds.
  cvenv no longer appends `.cvenv/` to the project's `.gitignore`. Existing envs get the file on
  their next build.
- Release binaries are now tested on both x86_64 and aarch64 before publishing, and a release
  fails if its tag doesn't match the version the binary reports.

### Fixed

- The release check that binaries are statically linked could never fail; it now rejects a
  dynamically linked binary.

## [0.4.2] - 2026-11-24

### Changed

- cvenv is now licensed under the Apache License, Version 2.0.

## [0.4.1] - 2026-11-24

### Fixed

- `cvenv self-update --force` on the installed version printed `updating cvenv 0.4.0 -> 0.4.0`;
  it now prints `reinstalling cvenv 0.4.0`. Installing an older release warns
  `downgrading cvenv A -> B` instead of `installing B over A`.

## [0.4.0] - 2026-11-24

### Added

- `cvenv self-update` installs the latest release if it is newer than the running version
  (`cvenv self update` is an alias). `--check` only reports whether an update is available,
  `--version VER` installs a specific release (including downgrades), and `--force` reinstalls
  the same version or replaces a binary built from a checkout.
- The downloaded binary is verified against its published sha256 checksum, run, and checked for
  the expected version before it atomically replaces the current one. On failure the installed
  binary is left untouched. Shims follow the new binary automatically.
- `self-update` refuses, with a hint, when the binary was built from a git checkout (unless
  `--force`) or its directory is not writable.
- `repo` setting for the GitHub repository releases come from (recorded by `installation.sh`),
  and `CVENV_RELEASES_URL` to download releases from a mirror.

Releases before 0.4.0 don't have `self-update`; update from them once by re-running the
installer.

## [0.3.0] - 2026-11-24

### Added

- squashfuse backend for clusters without a usable Apptainer: envs are built as a SquashFS image
  that is mounted with squashfuse (no root, no user namespaces) and commands run directly on the
  host. See [docs/backends.md](docs/backends.md).
- `backend` setting (`auto`, `apptainer` or `squashfuse`) and `--backend` on `rebuild` and
  `wrap-install`. `auto` (the default) uses Apptainer when it is installed and squashfuse
  otherwise. An env keeps its backend across rebuilds; existing envs stay on Apptainer.
- squashfuse mounts are created on demand by the shims, repaired when stale, and unmounted after
  `mount-timeout` seconds of inactivity (default 600). They live under `mount-root` (default
  `<store>/.cvenv-mnt`), so `cvenv move` needs no rebuild; old builds' mount points are removed
  after `mount-retention` days (default 14).
- Under Slurm, mounts go through `systemd-run --user` when the user's systemd instance is
  running, so a job ending doesn't break other jobs on the node that share the mount
  (`mount-mode = auto|systemd|direct`).
- `wrap-install` supports the squashfuse backend. `cvenv wrap` still requires Apptainer.
- `cvenv envs` shows a BACKEND column, and `cvenv info` shows the mount point and its state.
- The installer detects squashfuse and, when no backend is available, installs it into the home
  directory without root.

### Changed

- The installer asks three questions (env store, sbatch arguments on Slurm clusters, shell rc
  setup) followed by a single confirmation. Install prefix, binary source, tool paths, backend,
  build scratch and rebuild mode are chosen automatically and remain available as flags (listed
  under "Advanced" in `--help`).
- The installer installs missing pixi and squashfuse without asking, falls back to building from
  source when the release download fails inside a checkout, and keeps an existing cvenv block in
  the shell rc up to date with `--yes`.
- `--yes` replaces `--headless` in the installer; `--headless` is still accepted.
- Backend details, settings, per-env options and environment variables moved from the README to
  [docs/backends.md](docs/backends.md) and [docs/configuration.md](docs/configuration.md).

## [0.2.0] - 2026-11-19

First release.

### Added

- Pixi environments installed into a single Apptainer image, with commands run from the image and
  the host environment, working directory and GPUs (`--nv`/`--rocm`, auto-detected) passed
  through.
- Commands: `init`, `activate`/`deactivate`, `add`/`remove`, `rebuild`, `run`, `shell`, `envs`,
  `info`, `list`, `task`, `move`, `unregister` and `config`. pixi's own flags are passed through
  by `add`, `remove`, `task`, `run`, `shell` and `list`.
- Builds on node-local scratch with an atomic swap of the new image, and `cvenv rebuild --slurm`
  to build as a batch job.
- `cvenv rebuild --incremental` reuses the current image so only changed packages are installed,
  falling back to a full build when needed.
- `cvenv init --import` for `environment.yml` and `requirements.txt`, and `--manifest` to register
  an existing `pixi.toml` or `pyproject.toml`.
- `cvenv wrap` to use an existing container (`.sif`, `docker://`, `oras://`), and
  `cvenv wrap-install` to pack an existing installation such as a conda env into one file, with
  `--replace` keeping its original paths working.
- `cvenv share` and `cvenv register` to share envs with a group.
- Envs can be selected by path (`--env /path/to/env`) without a registry entry.
- Host-valid interpreter paths: `<env>/.cvenv/bin/python` works both inside the image and on the
  host, so tools that re-launch `sys.executable` (multiprocessing spawn, dask-jobqueue, ipykernel)
  work.
- Warnings when an env's manifest changed since its last build, and a guard against
  `pip install` into an env.
- `cvenv agent-guide`, a usage guide for AI coding agents built into the binary.
- Interactive installer, `installation.sh`, with a non-interactive mode.
- Static x86_64 and aarch64 Linux binaries attached to each GitHub release, with sha256
  checksums.

[Unreleased]: https://github.com/amarchakitus/cvenv/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/amarchakitus/cvenv/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/amarchakitus/cvenv/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/amarchakitus/cvenv/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/amarchakitus/cvenv/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/amarchakitus/cvenv/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/amarchakitus/cvenv/releases/tag/v0.2.0
