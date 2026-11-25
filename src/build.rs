//! Building an env's image: definition file generation + `apptainer build`.

use crate::config;
use crate::env::{
    compute_fingerprint, cuda_requirement, stat_sig, Backend, Env, Kind, Meta, BUILD_CFG_KEYS,
};
use crate::runtime::{
    find_apptainer, find_pixi, pixi_version, select_backend, write_layer, write_shims,
    HOST_ENV_DROP, IMG_ENVS, IMG_PIXI, IMG_ROOT,
};
use crate::util::{
    expand, human_size, info, lock_file, now_iso, q, qp, remove_tree, sha256_bytes, username, warn,
    write_json,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

/// (Bootstrap, From) for an image reference.
pub fn parse_base_image(r: &str) -> (String, String) {
    for kind in [
        "docker-daemon",
        "docker",
        "oras",
        "library",
        "shub",
        "localimage",
    ] {
        if let Some(rest) = r.strip_prefix(&format!("{}://", kind)) {
            return (kind.to_string(), rest.to_string());
        }
    }
    if r.ends_with(".sif") || expand(r).is_file() {
        return ("localimage".into(), expand(r).to_string_lossy().to_string());
    }
    ("docker".into(), r.to_string())
}

/// `incremental_from`: start from this existing image of the env (only the pixi
/// install delta is applied; post-commands already ran when it was built).
fn make_deffile(
    env: &Env,
    pixi_host: &Path,
    base: &str,
    update: bool,
    incremental_from: Option<&Path>,
) -> String {
    let (bootstrap, from) = match incremental_from {
        Some(img) => ("localimage".to_string(), img.display().to_string()),
        None => parse_base_image(base),
    };
    let post = if incremental_from.is_some() {
        vec![]
    } else {
        env.cfg_str_list("post-commands")
    };
    let proj = env.manifest_dir();
    let cuda = std::env::var("CONDA_OVERRIDE_CUDA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cuda_requirement(env));
    let mut l: Vec<String> = vec![
        format!("Bootstrap: {}", bootstrap),
        format!("From: {}", from),
        String::new(),
        "%labels".into(),
        format!("    cvenv.env {}", env.name),
        format!("    cvenv.pixi-env {}", env.pixi_env),
        format!("    cvenv.manifest {}", env.manifest.display()),
        format!("    cvenv.version {}", env!("CARGO_PKG_VERSION")),
        String::new(),
        "%setup".into(),
        "    set -e".into(),
        format!("    mkdir -p \"${{APPTAINER_ROOTFS}}\"{}", qp(&proj)),
        format!(
            "    mkdir -p \"${{APPTAINER_ROOTFS}}/cvenv-build/cache\" \"${{APPTAINER_ROOTFS}}/cvenv-build/out\" \"${{APPTAINER_ROOTFS}}{}/bin\"",
            IMG_ROOT
        ),
        format!("    cp {} \"${{APPTAINER_ROOTFS}}{}\"", qp(pixi_host), IMG_PIXI),
        format!("    chmod 755 \"${{APPTAINER_ROOTFS}}{}\"", IMG_PIXI),
        String::new(),
        "%post".into(),
        "    set -eu".into(),
        "    export HOME=/root".into(),
        "    export PIXI_CACHE_DIR=/cvenv-build/cache RATTLER_CACHE_DIR=/cvenv-build/cache UV_CACHE_DIR=/cvenv-build/cache/uv".into(),
        "    export PIXI_NO_PROGRESS=true".into(),
    ];
    if let Some(c) = cuda {
        // pixi checks [system-requirements] against the build machine, which usually has no GPU
        l.push(format!("    export CONDA_OVERRIDE_CUDA={}", q(&c)));
    }
    if !post.is_empty() {
        l.push("    # ---- [tool.cvenv] post-commands ----".into());
        l.extend(post.iter().map(|c| format!("    {}", c)));
        l.push("    # ------------------------------------".into());
    }
    l.extend([
        "    mkdir -p /etc/pixi".to_string(),
        format!(
            "    printf 'detached-environments = \"{}\"\\n' > /etc/pixi/config.toml",
            IMG_ENVS
        ),
        format!("    PIXI={}", IMG_PIXI),
        format!("    M={}", qp(&env.manifest)),
        format!("    E={}", q(&env.pixi_env)),
        "    OUT=/cvenv-build/out".to_string(),
        format!("    cd {}", qp(&proj)),
    ]);
    if update {
        l.push("    \"$PIXI\" update --manifest-path \"$M\"".into());
    }
    l.extend([
        "    \"$PIXI\" install --manifest-path \"$M\" -e \"$E\"".to_string(),
        format!("    PREFIX=$(ls -d {}/*/envs/\"$E\" | head -n 1)", IMG_ENVS),
        format!("    ln -sfn \"$PREFIX\" {}/env", IMG_ROOT),
        "    printf '%s\\n' \"$PREFIX\" > \"$OUT/prefix\"".to_string(),
        "    # Capture the pixi activation (incl. activate.d scripts) as a static env script".to_string(),
        "    BASE_PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "    _ENVI=\"env -i HOME=/root PATH=$BASE_PATH _CVENV_PIXI=$PIXI _CVENV_M=$M _CVENV_E=$E${CONDA_OVERRIDE_CUDA:+ CONDA_OVERRIDE_CUDA=$CONDA_OVERRIDE_CUDA}\"".to_string(),
        "    $_ENVI bash --noprofile --norc -c 'export -p' | sort > \"$OUT/env.before\"".to_string(),
        "    $_ENVI bash --noprofile --norc -c 'h=\"$(\"$_CVENV_PIXI\" shell-hook --frozen --manifest-path \"$_CVENV_M\" -e \"$_CVENV_E\" --shell bash)\" || exit 1; eval \"$h\"; unset CONDA_OVERRIDE_CUDA; export -p' > \"$OUT/env.after.raw\"".to_string(),
        "    sort \"$OUT/env.after.raw\" > \"$OUT/env.after\"".to_string(),
        "    grep -q '^declare -x CONDA_PREFIX=' \"$OUT/env.after\" || { echo 'cvenv: capturing the pixi activation failed' >&2; exit 1; }".to_string(),
        "    {".to_string(),
        "      echo '# cvenv: pixi activation of environment '\"$E\"' (generated at build time)'".to_string(),
        "      comm -13 \"$OUT/env.before\" \"$OUT/env.after\" | grep -E '^declare -x [A-Za-z_][A-Za-z0-9_]*=' | grep -Ev '^declare -x (PWD|OLDPWD|SHLVL|_|HOME|PS1|_CVENV_[A-Z]+)=' | sed 's/^declare -x /export /'".to_string(),
        format!("      echo 'export PATH=\"$PATH:{}/bin\"'", IMG_ROOT),
        "    } > /.singularity.d/env/91-cvenv.sh".to_string(),
        "    chmod 644 /.singularity.d/env/91-cvenv.sh".to_string(),
        "    cp /.singularity.d/env/91-cvenv.sh \"$OUT/activate.sh\"".to_string(),
        "    ls -1 \"$PREFIX/bin\" > \"$OUT/bin-list\" 2>/dev/null || true".to_string(),
        "    \"$PIXI\" list --frozen --manifest-path \"$M\" -e \"$E\" > \"$OUT/packages.txt\" 2>&1 || true".to_string(),
        format!("    mkdir -p {}/src", IMG_ROOT),
        format!("    cp \"$M\" {}/src/", IMG_ROOT),
        format!("    if [ -f pixi.lock ]; then cp pixi.lock {}/src/; fi", IMG_ROOT),
        format!("    cp \"$OUT/packages.txt\" {}/src/ 2>/dev/null || true", IMG_ROOT),
        "    rm -rf /root/.cache".to_string(),
        String::new(),
        "%runscript".to_string(),
        "    if [ \"$#\" -eq 0 ]; then exec /bin/bash; fi".to_string(),
        "    exec \"$@\"".to_string(),
        String::new(),
    ]);
    l.join("\n")
}

/// `<build-tmpdir>/cvenv-$USER`: node-local scratch for builds and caches.
pub fn build_tmp_root() -> Result<PathBuf> {
    let base = config::value("build-tmpdir")
        .map(|v| expand(&v))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let d = base.join(format!("cvenv-{}", username()));
    if !d.is_dir() {
        fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
    }
    Ok(d)
}

/// The bind-path variable for `app` (SingularityCE reads SINGULARITY_BINDPATH; setting both
/// makes Apptainer complain) and its value: binds already set by the site or user, then ours.
fn build_bind_path(app: &Path, binds: &[String]) -> (&'static str, String) {
    let singularity = app
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("singularity"));
    let var = if singularity {
        "SINGULARITY_BINDPATH"
    } else {
        "APPTAINER_BINDPATH"
    };
    let existing = std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    let value = existing
        .into_iter()
        .chain(binds.iter().cloned())
        .collect::<Vec<_>>()
        .join(",");
    (var, value)
}

fn default_base_image(pixi_host: &Path) -> String {
    if let Some(v) = config::value("base-image") {
        return v;
    }
    format!(
        "docker://ghcr.io/prefix-dev/pixi:{}",
        pixi_version(pixi_host).unwrap_or_else(|| "latest".into())
    )
}

/// Hash of what the image's base layer depends on (base image + build-relevant settings).
fn build_cfg_hash(env: &Env, base: &str) -> String {
    let cfg = env.cfg();
    let mut m = BTreeMap::new();
    for k in BUILD_CFG_KEYS {
        if let Some(v) = cfg.get(*k) {
            m.insert(k.to_string(), v.to_string());
        }
    }
    m.insert("__base__".into(), base.to_string());
    sha256_bytes(serde_json::to_string(&m).unwrap_or_default().as_bytes())
}

/// Why an incremental build is not possible (None = it is).
fn incremental_blocker(env: &Env, base_hash: &str) -> Option<String> {
    if !env.built() {
        return Some("there is no previous image".into());
    }
    let meta = match env.meta() {
        Some(m) => m,
        None => return Some("the previous image has no build metadata".into()),
    };
    if meta.build_cfg.as_deref() != Some(base_hash) {
        return Some(
            "the base image or post-commands changed (or the image predates incremental support)"
                .into(),
        );
    }
    if crate::util::normalize(&meta.manifest) != crate::util::normalize(&env.manifest) {
        return Some("the env was moved since the last build".into());
    }
    if meta.pixi_env != env.pixi_env {
        return Some("the pixi environment changed".into());
    }
    None
}

/// Run `cmd`, copying its stdout+stderr to `log` and to our stderr.
pub fn run_logged(cmd: &mut Command, log: &mut fs::File) -> Result<ExitStatus> {
    let (reader, writer) = std::io::pipe()?;
    cmd.stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let prog = cmd.get_program().to_string_lossy().to_string();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not run {}", prog))?;
    // close our copies of the pipe's write end (they live in `cmd`)
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let mut stderr = std::io::stderr();
    for line in BufReader::new(reader).split(b'\n') {
        let mut line = line?;
        line.push(b'\n');
        let _ = log.write_all(&line);
        let _ = stderr.write_all(&line);
    }
    Ok(child.wait()?)
}

pub fn build_env(
    env: &Env,
    update: bool,
    fresh_cache: bool,
    keep_build_dir: bool,
    incremental: bool,
    backend: Option<Backend>,
) -> Result<()> {
    if env.unregistered {
        bail!(
            "env '{}' belongs to another user (or isn't registered); only its owner can rebuild it",
            env.name
        );
    }
    match env.kind {
        Kind::Wrap => return crate::wrap::refresh_wrap(env),
        Kind::WrapInstall => return crate::wrap::refresh_install(env, backend),
        Kind::Pixi => {}
    }
    if select_backend(Some(env), backend, Kind::Pixi)? == Backend::Squashfuse {
        return crate::squash::build(env, update, fresh_cache, keep_build_dir, incremental);
    }
    let app = find_apptainer()?;
    let pixi_host = find_pixi()?;
    let previous = env.meta();
    if !env.manifest.is_file() {
        bail!("manifest not found: {}", env.manifest.display());
    }
    env.ensure_art()?;
    let _lock = lock_file(
        &env.art().join(".build.lock"),
        false,
        &format!("env '{}' (another build is running?)", env.name),
    )?;

    let base = env
        .cfg()
        .get("base-image")
        .map(crate::env::toml_to_string)
        .unwrap_or_else(|| default_base_image(&pixi_host));
    let tmp_root = build_tmp_root()?;
    let work = tmp_root.join(format!("build-{}-{}", env.name, std::process::id()));
    remove_tree(&work);
    fs::create_dir_all(work.join("out"))?;
    let out = work.join("out");
    let cache = if fresh_cache {
        work.join("pixi-cache")
    } else {
        config::value("pixi-cache")
            .map(|v| expand(&v))
            .unwrap_or_else(|| tmp_root.join("pixi-cache"))
    };
    fs::create_dir_all(&cache)?;

    let base_hash = build_cfg_hash(env, &base);
    // copy of the current image to build from (the build replaces image.sif at the end)
    let mut from_image: Option<PathBuf> = None;
    if incremental {
        match incremental_blocker(env, &base_hash) {
            None => from_image = Some(env.image()),
            Some(why) => info(&format!(
                "incremental build not possible ({}); doing a full build",
                why
            )),
        }
    }
    let deffile = work.join("cvenv.def");
    let def = make_deffile(env, &pixi_host, &base, update, from_image.as_deref());
    fs::write(&deffile, &def)?;

    let partial = PathBuf::from(format!("{}.partial", env.image().display()));
    let log_path = env.art().join("build.log");
    let args: Vec<String> = vec![
        "build".into(),
        "--force".into(),
        partial.to_string_lossy().to_string(),
        deffile.to_string_lossy().to_string(),
    ];
    // Binds for %post go through APPTAINER_BINDPATH, not `build --bind`: Apptainer passes
    // `build --bind` paths to the nested %post exec both as -B and as APPTAINER_BINDPATH,
    // so each would be mounted twice ("destination is already in the mount point list").
    let (bind_var, bind_path) = build_bind_path(
        &app,
        &[
            env.manifest_dir().to_string_lossy().to_string(),
            format!("{}:/cvenv-build/cache", cache.display()),
            format!("{}:/cvenv-build/out", out.display()),
        ],
    );

    info(&format!(
        "building env '{}' (pixi env '{}'){}",
        env.name,
        env.pixi_env,
        if from_image.is_some() {
            " incrementally from the current image"
        } else {
            ""
        }
    ));
    info(&format!("  manifest   : {}", env.manifest.display()));
    info(&format!("  base image : {}", base));
    info(&format!("  build dir  : {}", work.display()));
    info(&format!("  image      : {}", env.image().display()));
    let t0 = Instant::now();

    let mut log =
        fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    writeln!(
        log,
        "# {}\n# {}={} {} {}\n\n{}\n# ---- build output ----",
        now_iso(),
        bind_var,
        bind_path,
        app.display(),
        args.join(" "),
        def
    )?;

    let mut cmd = Command::new(&app);
    cmd.args(&args)
        .current_dir(&work)
        // Apptainer unpacks the base image and runs the build in APPTAINER_TMPDIR:
        // keep those (many) files on node-local disk, never on the quota'd filesystem.
        .env("APPTAINER_TMPDIR", &work)
        .env("SINGULARITY_TMPDIR", &work)
        .env("TMPDIR", &work)
        .env(bind_var, &bind_path);
    for k in HOST_ENV_DROP {
        cmd.env_remove(k);
    }
    let status = run_logged(&mut cmd, &mut log)?;
    if !status.success() {
        let _ = fs::remove_file(&partial);
        if !keep_build_dir {
            remove_tree(&work);
        }
        bail!(
            "build failed ({}); the previous image (if any) is untouched. Log: {}",
            status,
            log_path.display()
        );
    }

    // Atomic swap: running jobs keep using the old (now unlinked) image.
    fs::rename(&partial, env.image())?;
    let bins: Vec<String> = match fs::read_to_string(out.join("bin-list")) {
        Ok(s) => s
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => {
            warn("could not read the list of executables from the build");
            vec![]
        }
    };
    for f in ["packages.txt", "activate.sh"] {
        let src = out.join(f);
        if src.is_file() {
            fs::copy(&src, env.art().join(f))?;
        }
    }

    let image_size = fs::metadata(env.image())?.len();
    let fresh = Env::new(&env.name, &env.record()); // re-read the manifest: the build may have touched it
    let prefix = fs::read_to_string(out.join("prefix"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let exec_paths: BTreeMap<String, String> = match &prefix {
        Some(p) => bins
            .iter()
            .map(|b| (b.clone(), format!("{}/bin/{}", p, b)))
            .collect(),
        None => BTreeMap::new(),
    };
    let meta = Meta {
        kind: Kind::Pixi,
        prefix: prefix.clone(),
        exec_paths: exec_paths.clone(),
        build_cfg: Some(base_hash.clone()),
        name: env.name.clone(),
        pixi_env: env.pixi_env.clone(),
        manifest: env.manifest.clone(),
        built_at: now_iso(),
        build_seconds: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        base_image: base.clone(),
        pixi_version: pixi_version(&pixi_host),
        apptainer: app.to_string_lossy().to_string(),
        cvenv_version: env!("CARGO_PKG_VERSION").into(),
        image_size,
        fingerprint: compute_fingerprint(&fresh)?,
        stat: stat_sig(&fresh),
        bins: bins.clone(),
        ..Default::default()
    };
    write_json(&env.meta_path(), &meta)?;
    write_layer(env, &exec_paths)?;
    env.write_env_file()?;
    let n = write_shims(env, &bins)?;
    // switched from the squashfuse backend: drop its images, retire its mount point
    crate::squash::remove_artifacts(env, None);
    crate::squash::retire_image(previous.as_ref());
    if !keep_build_dir {
        remove_tree(&work);
    }
    info(&format!(
        "built '{}' in {:.0}s: {} ({}, {} commands shimmed)",
        env.name,
        t0.elapsed().as_secs_f64(),
        env.image().display(),
        human_size(image_size),
        n
    ));
    Ok(())
}

/// State of a submitted Slurm build ("PENDING", "RUNNING", ...) if it is still queued/running.
pub fn slurm_build_state(env: &Env) -> Option<(String, String)> {
    let job = fs::read_to_string(env.art().join("slurm-job"))
        .ok()?
        .trim()
        .to_string();
    if job.is_empty() {
        return None;
    }
    let out = Command::new(crate::util::which("squeue")?)
        .args(["-h", "-j", &job, "-o", "%T"])
        .output()
        .ok()?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state.is_empty() {
        None
    } else {
        Some((job, state))
    }
}

/// True while a build runs: a local build holding the lock, or a queued/running Slurm job
/// (flock is not reliable across nodes on parallel filesystems, so jobs are tracked by id).
pub fn is_building(env: &Env) -> bool {
    if slurm_build_state(env).is_some() {
        return true;
    }
    let p = env.art().join(".build.lock");
    if !p.exists() {
        return false;
    }
    match fs::OpenOptions::new().append(true).open(&p) {
        Ok(f) => {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
                false
            } else {
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
            }
        }
        Err(_) => false,
    }
}

/// Submit `cvenv rebuild --local` for `env` as a Slurm batch job.
pub fn submit_slurm(
    env: &Env,
    update: bool,
    fresh_cache: bool,
    incremental: bool,
    backend: Option<Backend>,
    extra: Option<&str>,
    wait: bool,
) -> Result<()> {
    let sbatch = crate::util::which("sbatch")
        .ok_or_else(|| anyhow::anyhow!("sbatch not found: Slurm builds need a Slurm cluster"))?;
    if let Some((job, state)) = slurm_build_state(env) {
        bail!(
            "env '{}' is already being built by Slurm job {} ({})",
            env.name,
            job,
            state
        );
    }
    if is_building(env) {
        bail!("env '{}' is already being built", env.name);
    }
    env.ensure_art()?;
    let exe = crate::config::exe_path();
    let mut cmd_line = format!("{} rebuild --local --env {}", qp(&exe), q(&env.name));
    if update {
        cmd_line.push_str(" --update");
    }
    if fresh_cache {
        cmd_line.push_str(" --no-cache");
    }
    if incremental {
        cmd_line.push_str(" --incremental");
    }
    if let Some(b) = backend {
        cmd_line.push_str(&format!(" --backend {}", b.label()));
    }
    let script = env.art().join("build-job.sh");
    let log = env.art().join("build-%j.log");
    let body = format!(
        "#!/bin/bash\n\
         #SBATCH --job-name=cvenv-build-{name}\n\
         #SBATCH --output={log}\n\
         # generated by `cvenv rebuild --slurm`\n\
         echo \"cvenv: building '{name}' on $(hostname) (job $SLURM_JOB_ID)\"\n\
         exec {cmd}\n",
        name = env.name,
        log = log.display(),
        cmd = cmd_line
    );
    crate::util::write_atomic(&script, body.as_bytes(), Some(0o755))?;

    // defaults < `slurm-args` config < --slurm-args
    let mut args: Vec<String> = vec![
        "--parsable".into(),
        "--cpus-per-task=8".into(),
        "--mem=32G".into(),
        "--time=02:00:00".into(),
    ];
    if let Some(a) = crate::config::value("slurm-args") {
        args.extend(a.split_whitespace().map(String::from));
    }
    if let Some(a) = extra {
        args.extend(a.split_whitespace().map(String::from));
    }
    if wait {
        args.push("--wait".into());
    }
    args.push(script.to_string_lossy().to_string());

    info(&format!(
        "submitting build of '{}': sbatch {}",
        env.name,
        args.join(" ")
    ));
    let mut child = Command::new(&sbatch)
        .args(&args)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", sbatch.display()))?;
    // with --parsable the job id is the first line; print it before --wait blocks
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    out.read_line(&mut first)?;
    let job = first.trim().split(';').next().unwrap_or("").to_string();
    if !job.is_empty() {
        let _ = fs::write(env.art().join("slurm-job"), format!("{}\n", job));
        let logfile = env.art().join(format!("build-{}.log", job));
        info(&format!(
            "job {} submitted; log: {}",
            job,
            logfile.display()
        ));
        if wait {
            info("waiting for the job to finish (Ctrl-C stops waiting; the job keeps running)");
        } else {
            info(&format!(
                "follow it with: tail -f {}   (`cvenv envs` shows 'building' meanwhile)",
                logfile.display()
            ));
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!(
            "sbatch failed ({}){}",
            status,
            if job.is_empty() {
                ""
            } else {
                "; see the job log"
            }
        );
    }
    if wait {
        info(&format!("build job {} finished", job));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_path_variable_and_value() {
        let binds = ["/a".to_string(), "/c:/cvenv-build/cache".to_string()];
        // the variable matches the executable; nothing set beforehand in the test env
        std::env::remove_var("APPTAINER_BINDPATH");
        std::env::remove_var("SINGULARITY_BINDPATH");
        let (v, val) = build_bind_path(Path::new("/usr/bin/apptainer"), &binds);
        assert_eq!(
            (v, val.as_str()),
            ("APPTAINER_BINDPATH", "/a,/c:/cvenv-build/cache")
        );
        let (v, _) = build_bind_path(Path::new("/opt/bin/singularity"), &binds);
        assert_eq!(v, "SINGULARITY_BINDPATH");
    }
}
