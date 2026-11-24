# Configuration

## Settings

Show all settings and where each value comes from with `cvenv config`. Change them with
`cvenv config set KEY VALUE` (stored in `~/.config/cvenv/config.json`).

| Key | Meaning |
|---|---|
| `store` | Directory in which new envs are created |
| `backend` | `auto` (default), `apptainer` or `squashfuse` |
| `rebuild-mode` | Where `cvenv rebuild` builds by default: `local` or `slurm` |
| `slurm-args` | sbatch arguments for Slurm builds, e.g. `-A myacct -p mypart` |
| `build-tmpdir` | Node-local scratch directory for builds (default `/tmp`) |
| `pixi-cache` | Persistent package cache for builds |
| `base-image` | Default base image for Apptainer builds |
| `apptainer`, `squashfuse`, `pixi` | Tool paths (set by the installer) |
| `repo` | GitHub repo that `cvenv self-update` installs releases from (default `amarchakitus/cvenv`) |
| `mount-timeout` | Seconds before an unused squashfuse mount is unmounted (default 600) |
| `mount-mode` | squashfuse mounts: `auto` (systemd user unit when available), `systemd` or `direct` |
| `mount-root` | Where squashfuse images are mounted (default `<store>/.cvenv-mnt`) |
| `mount-retention` | Days an old build's mount point is kept before removal (default 14) |

Every key can also be set for a single command with an environment variable:
`CVENV_` followed by the key in upper case with `-` replaced by `_` (for example `CVENV_STORE`).

## Per-env options

Pixi envs take options in their manifest under `[tool.cvenv]`; wrapped envs use
`<env>/cvenv.toml` with the same keys at the top level.

```toml
[tool.cvenv]
gpu = "auto"                  # auto | true | false | rocm         (apptainer)
binds = ["/data", "/in:/mnt:ro"]                                  # (apptainer)
env = { OMP_NUM_THREADS = "8" }
base-image = "docker://..."   # requires a rebuild                 (apptainer)
post-commands = ["..."]       # requires a rebuild                 (apptainer)
apptainer-args = []                                               # (apptainer)
shim-exclude = []             # commands not to expose
shim-include = []
host-valid-paths = true
isolate-user-site = true      # PYTHONNOUSERSITE=1
```

## Environment variables

| Variable | Effect |
|---|---|
| `CVENV_GPU` | Override `gpu` for one command |
| `CVENV_BINDS` | Extra bind mounts, comma-separated (apptainer) |
| `CVENV_APPTAINER_ARGS` | Extra arguments for `apptainer` |
| `CVENV_NO_STALE_WARNING` | Don't warn about envs whose manifest changed since the build |
| `CVENV_NO_PROMPT` | Don't prefix the shell prompt on `cvenv activate` |
| `CVENV_DEBUG` | Print the container command or the squashfuse mount and exec |
| `CVENV_CONFIG_DIR` | Use another configuration directory |
| `CVENV_RELEASES_URL` | Download `cvenv self-update` releases from this URL instead of GitHub (e.g. a mirror) |
