# Backends

cvenv stores each env as one image file and supports two ways of building and running it.

|   | apptainer | squashfuse |
|---|---|---|
| Image | `.cvenv/image.sif` | `.cvenv/image-<build>.sqfs` |
| Requires | Apptainer (or Singularity) | FUSE and squashfuse; no root, no user namespaces |
| Commands run | in a container (OS from the base image) | on the host (host OS, all host paths) |
| Host commands (`sbatch`, `module`, …) | not available | available |
| Start-up cost | about 0.5 s per command | about 20 ms once mounted; mounting takes under 1 s |
| GPUs | `--nv` / `--rocm` added automatically | host drivers used directly |
| `cvenv wrap` (existing containers) | supported | not supported |

`backend = auto` (the default) uses Apptainer when it is installed and squashfuse otherwise.
Choose explicitly per build with `cvenv rebuild --backend apptainer|squashfuse`, or set a default
with `cvenv config set backend ...`. An env keeps its backend on later rebuilds unless told
otherwise.

## Apptainer

Commands run inside the container with your environment variables, working directory and GPUs
passed through. Visible paths are `$HOME`, `/tmp`, the current directory and the site's default
bind paths (for example `/project` and `/scratch`). Add more with `CVENV_BINDS=/a,/b` or
`binds = [...]` under `[tool.cvenv]`.

## squashfuse

### Building

pixi installs the env through its future mount point, which during the build is a symlink to
node-local scratch. Every path recorded in the env points at the mount point, while the files
themselves land in scratch; the tree is then squashed into a single image file. The env's files
never touch the shared filesystem.

### Mounting

The shims and `cvenv run` mount the image on demand, once per node and user. A mount that has
been unused for `mount-timeout` seconds (default 600) unmounts itself; it never does so while
files are open. Stale mounts, such as those left by killed jobs, are detected and cleaned up
automatically.

Under Slurm, all of a job's processes are killed when it ends, which would break other jobs on
the same node that share the mount. When your systemd user instance runs on the node, cvenv
therefore mounts through `systemd-run --user`, so the mount outlives the job. Set
`mount-mode = direct` to mount from the job itself instead.

### Mount points

Mount points live under `mount-root` (default `<store>/.cvenv-mnt/<env-id>/<build>/`), outside
the env directory. As a result:

- `cvenv move` never touches a mounted directory, and moved envs keep working without a rebuild.
- After a rebuild or `cvenv unregister --delete`, the old mount point is kept for
  `mount-retention` days (default 14), because jobs on other nodes may still use it. It is then
  removed if it is empty and not mounted on the node doing the cleanup.

Removing or renaming a directory that is mounted on another node leaves a dead mount entry on that
node that only root can clear, so never delete anything under the mount root by hand.

### Limitations

- FUSE mounts belong to the user who made them. Two users can't use the same shared env on the
  same node at the same time.
- `sys.executable` and other recorded paths point into the mount point, which exists only on
  nodes where the env is mounted. To start a process on another node, use the shim
  (`<env>/.cvenv/bin/python`), which mounts the image first.
- Don't copy env directories by hand (`cp -r`): the copy shares the original's mount area. Use
  `cvenv init NAME --manifest /path/to/pixi.toml` instead.
