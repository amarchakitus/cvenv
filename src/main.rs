//! cvenv - pixi environments packed into single-file Apptainer images.
//!
//! Every env is an ordinary pixi workspace (pixi.toml + pixi.lock). Instead of
//! materialising the environment on the shared filesystem (tens of thousands of
//! files), `cvenv rebuild` installs it inside an Apptainer image (one .sif file).
//! Activation puts a directory of shims (python, jupyter, ...) on PATH; each shim
//! runs the command inside the container with the host environment, working
//! directory, bind mounts and GPUs passed through. The shims are symlinks to this
//! binary, which dispatches on argv[0].

mod build;
mod config;
mod env;
mod runtime;
mod util;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use env::{env_for_shim_dir, find_manifest, load_registry, resolve_env, staleness, update_registry, valid_name, warn_if_stale, Env, Kind, Record, SHIM_LINK};
use runtime::{exec_in_container, find_pixi, gpu_flags, IMG_PIXI};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use util::{die, expand, human_size, info, is_under, now_iso, remove_tree, stdin_is_tty, stdout_is_tty, warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Usage guide for AI agents, compiled into the binary (`cvenv agent-guide`).
const AGENT_GUIDE: &str = include_str!("../docs/AGENTS.md");

const HELP: &str = "\
cvenv - pixi environments in single-file Apptainer images

Environment management
  init NAME [--path DIR] [--manifest FILE] [-c CH] [--cuda VER] [PKG...]
                          create (or adopt) a pixi workspace and register it
  envs                    list registered envs and their build status
  info [--env NAME]       details about an env
  move NAME DEST          relocate an env directory
  unregister NAME [--delete [--yes]]
                          forget an env (optionally deleting its files)
  config [list|get|set|unset] [KEY] [VALUE]
                          settings (store, apptainer, pixi, build-tmpdir, ...)

Working with an env (uses the active env unless --env NAME|PATH is given)
  activate [NAME]         put the env's commands on PATH in this shell
  deactivate              undo activate
  add [pixi add args]     add dependencies to the manifest (no build)
  remove [pixi remove args]
                          remove dependencies from the manifest (no build)
  rebuild [--update] [--no-cache] [--slurm [--wait] [--slurm-args ARGS] | --local]
                          (re)build the image from the manifest (here or as a Slurm job)
  run CMD [ARGS...]       run a command (or pixi task) inside the env
  exec CMD [ARGS...]      like run, but never interprets CMD as a pixi task
  shell                   interactive shell inside the container
  list [REGEX]            packages installed in the image
  task [pixi task args]   manage pixi tasks in the manifest

Setup
  shell-init              print shell code for ~/.bashrc:  eval \"$(cvenv shell-init)\"
  agent-guide             print the usage guide for AI agents (rules, recipes, troubleshooting)

Environment variables: CVENV_GPU=auto|1|0|rocm, CVENV_BINDS=a,b:c, CVENV_APPTAINER_ARGS,
CVENV_NO_STALE_WARNING=1, CVENV_NO_PROMPT=1, CVENV_DEBUG=1, CVENV_STORE, CVENV_CONFIG_DIR.
";

#[derive(Parser)]
#[command(name = "cvenv", version, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create (or adopt) a pixi workspace and register it as an env
    Init {
        name: String,
        /// Env directory (default: <store>/NAME)
        #[arg(long)]
        path: Option<String>,
        /// Register an existing pixi.toml/pyproject.toml (or its directory)
        #[arg(long)]
        manifest: Option<String>,
        /// pixi environment to build (default: "default")
        #[arg(long)]
        pixi_env: Option<String>,
        /// Channel for a new manifest (repeatable; default conda-forge)
        #[arg(short, long)]
        channel: Vec<String>,
        /// Platform for a new manifest (repeatable; default linux-64)
        #[arg(short, long)]
        platform: Vec<String>,
        /// Add `[system-requirements] cuda = VER` (needed to resolve GPU builds)
        #[arg(long)]
        cuda: Option<String>,
        /// Packages to add to a new manifest
        packages: Vec<String>,
    },
    /// List registered envs
    Envs,
    /// Show details about an env
    Info {
        #[arg(long, short = 'n')]
        env: Option<String>,
    },
    /// Build the env's image from its manifest
    #[command(alias = "build")]
    Rebuild {
        #[arg(long, short = 'n')]
        env: Option<String>,
        /// Re-solve dependencies (`pixi update`) before installing
        #[arg(long)]
        update: bool,
        /// Use a fresh, temporary package cache
        #[arg(long)]
        no_cache: bool,
        /// Build in a Slurm batch job (default when `rebuild-mode = slurm`)
        #[arg(long, conflicts_with = "local")]
        slurm: bool,
        /// Build right here, even if `rebuild-mode = slurm`
        #[arg(long)]
        local: bool,
        /// Extra sbatch arguments (added after the `slurm-args` config)
        #[arg(long, value_name = "ARGS", allow_hyphen_values = true)]
        slurm_args: Option<String>,
        /// With --slurm: wait for the job to finish (sbatch --wait)
        #[arg(long)]
        wait: bool,
        #[arg(long, hide = true)]
        keep_build_dir: bool,
    },
    /// Activate an env in the current shell (prints shell code)
    Activate { name: Option<String> },
    /// Deactivate the active env (prints shell code)
    Deactivate,
    /// Move an env directory
    #[command(alias = "mv")]
    Move { name: String, dest: String },
    /// Forget an env
    Unregister {
        name: String,
        /// Also delete the image (and the env directory if cvenv owns the manifest)
        #[arg(long)]
        delete: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Show or change settings
    Config {
        action: Option<String>,
        key: Option<String>,
        #[arg(allow_hyphen_values = true)]
        value: Option<String>,
    },
    /// Print shell integration code
    ShellInit { shell: Option<String> },
}

fn main() {
    // behave like a normal CLI when piped into `head` etc.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().cloned().unwrap_or_default();
    let result = if let Some((shim_dir, prog)) = runtime::shim_target(&argv0) {
        shim_main(&shim_dir, &prog, &args[1..])
    } else {
        dispatch(&args[1..])
    };
    if let Err(e) = result {
        util::error_line(&format!("{:#}", e));
        std::process::exit(1);
    }
}

fn dispatch(argv: &[String]) -> Result<()> {
    let first = argv.first().map(String::as_str);
    match first {
        None | Some("-h") | Some("--help") | Some("help") => {
            print!("{}", HELP);
            return Ok(());
        }
        Some("agent-guide") => {
            print!("{}", AGENT_GUIDE);
            return Ok(());
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("cvenv {}", VERSION);
            return Ok(());
        }
        // pass-through commands: hand-parsed so pixi's own flags flow through
        Some("add") => return cmd_add_remove("add", &argv[1..]),
        Some("remove") | Some("rm") => return cmd_add_remove("remove", &argv[1..]),
        Some("task") => return cmd_task(&argv[1..]),
        Some("list") => return cmd_list(&argv[1..]),
        Some("run") => return cmd_run(&argv[1..], false),
        Some("exec") => return cmd_run(&argv[1..], true),
        Some("shell") => return cmd_shell(&argv[1..]),
        _ => {}
    }
    let cli = Cli::parse_from(std::iter::once("cvenv".to_string()).chain(argv.iter().cloned()));
    match cli.cmd {
        Cmd::Init { name, path, manifest, pixi_env, channel, platform, cuda, packages } => {
            cmd_init(&name, path, manifest, pixi_env, channel, platform, cuda, packages)
        }
        Cmd::Envs => cmd_envs(),
        Cmd::Info { env } => cmd_info(env.as_deref()),
        Cmd::Rebuild { env, update, no_cache, slurm, local, slurm_args, wait, keep_build_dir } => {
            let e = resolve_env(env.as_deref(), true, true)?;
            let mode_slurm = config::value("rebuild-mode").map(|m| m.trim().eq_ignore_ascii_case("slurm")).unwrap_or(false);
            // inside a Slurm job (e.g. the submitted build itself) always build locally
            let in_job = std::env::var_os("SLURM_JOB_ID").is_some();
            if slurm || (mode_slurm && !local && !in_job) {
                build::submit_slurm(&e, update, no_cache, slurm_args.as_deref(), wait)
            } else {
                if !in_job && !local && util::which("sbatch").is_some() {
                    info("building on this node; for heavy envs use `cvenv rebuild --slurm` (or `cvenv config set rebuild-mode slurm`)");
                }
                build::build_env(&e, update, no_cache, keep_build_dir)
            }
        }
        Cmd::Activate { name } => cmd_activate(name.as_deref()),
        Cmd::Deactivate => cmd_deactivate(),
        Cmd::Move { name, dest } => cmd_move(&name, &dest),
        Cmd::Unregister { name, delete, yes } => cmd_unregister(&name, delete, yes),
        Cmd::Config { action, key, value } => cmd_config(action.as_deref(), key.as_deref(), value.as_deref()),
        Cmd::ShellInit { .. } => {
            print!("{}", runtime::shell_init());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// shims
// ---------------------------------------------------------------------------

fn is_pip(prog: &str) -> bool {
    prog == "pip" || prog == "pip3" || prog.strip_prefix("pip3.").map(|v| v.chars().all(|c| c.is_ascii_digit())).unwrap_or(false)
}

fn shim_main(shim_dir: &Path, prog: &str, args: &[String]) -> Result<()> {
    let env = match env_for_shim_dir(shim_dir) {
        Some(e) => e,
        None => bail!("{}: the env owning {} is not registered (moved or unregistered?)", prog, shim_dir.display()),
    };
    if is_pip(prog) && args.first().map(|a| a == "install").unwrap_or(false) && std::env::var_os("CVENV_ALLOW_PIP").is_none() {
        let pkgs: Vec<&str> = args[1..].iter().map(String::as_str).filter(|a| !a.starts_with('-')).collect();
        bail!(
            "`pip install` cannot modify the read-only image (and would litter ~/.local).\n    \
             Use `cvenv add --pypi {}` and then `{}`.\n    (set CVENV_ALLOW_PIP=1 to run pip anyway)",
            pkgs.join(" "),
            env.rebuild_hint()
        );
    }
    if !env.built() {
        bail!("env '{}' has not been built yet - run `{}`", env.name, env.rebuild_hint());
    }
    if (prog == "pip" || prog == "pip3") && !env.meta().map(|m| m.bins.iter().any(|b| b == prog)).unwrap_or(false) {
        bail!(
            "env '{}' has no {}. Add packages with `cvenv add --pypi PKG` (or `cvenv add pip`), then `{}`",
            env.name,
            prog,
            env.rebuild_hint()
        );
    }
    warn_if_stale(&env);
    let mut argv = vec![prog.to_string()];
    argv.extend(args.iter().cloned());
    exec_in_container(&env, &argv, "exec")
}

// ---------------------------------------------------------------------------
// env management
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_init(
    name: &str,
    path: Option<String>,
    manifest: Option<String>,
    pixi_env: Option<String>,
    channels: Vec<String>,
    platforms: Vec<String>,
    cuda: Option<String>,
    packages: Vec<String>,
) -> Result<()> {
    if !valid_name(name) {
        bail!("invalid env name '{}' (use letters, digits, '.', '_' and '-')", name);
    }
    let reg = load_registry();
    if let Some(r) = reg.envs.get(name) {
        bail!("env '{}' already exists at {}", name, r.path.display());
    }
    let path = path.map(|p| expand(&p)).unwrap_or_else(|| config::store_dir().join(name));
    for (n, r) in &reg.envs {
        if util::normalize(&r.path) == util::normalize(&path) {
            bail!("{} is already used by env '{}'", path.display(), n);
        }
    }

    let (manifest, created) = if let Some(m) = manifest {
        let mf = find_manifest(Path::new(&m)).ok_or_else(|| anyhow::anyhow!("no pixi.toml/pyproject.toml found at {}", m))?;
        fs::create_dir_all(&path)?;
        (mf, false)
    } else if let Some(mf) = if path.is_dir() { find_manifest(&path) } else { None } {
        info(&format!("using existing manifest {}", mf.display()));
        (mf, false)
    } else {
        let pixi = find_pixi()?;
        fs::create_dir_all(&path)?;
        let mut cmd = Command::new(&pixi);
        cmd.arg("init").arg(&path).args(["--format", "pixi"]);
        let channels = if channels.is_empty() { vec!["conda-forge".to_string()] } else { channels };
        let platforms = if platforms.is_empty() { vec!["linux-64".to_string()] } else { platforms };
        for c in &channels {
            cmd.args(["-c", c]);
        }
        for p in &platforms {
            cmd.args(["-p", p]);
        }
        if !cmd.status()?.success() {
            bail!("`pixi init` failed");
        }
        let mf = path.join("pixi.toml");
        if let Some(c) = &cuda {
            let mut s = fs::read_to_string(&mf)?;
            s.push_str(&format!("\n[system-requirements]\ncuda = \"{}\"\n", c));
            fs::write(&mf, s)?;
        }
        (mf, true)
    };
    if cuda.is_some() && !created {
        warn("--cuda only applies to newly created manifests; add [system-requirements] by hand");
    }

    // keep images/shims out of git (only relevant when the workspace holds .cvenv/)
    let gi = manifest.parent().unwrap_or(&path).join(".gitignore");
    let existing = fs::read_to_string(&gi).unwrap_or_default();
    if is_under(&manifest, &path) && !existing.split_whitespace().any(|l| l == ".cvenv/") {
        let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
        let _ = fs::write(&gi, format!("{}{}# cvenv images, shims and logs\n.cvenv/\n", existing, sep));
    }

    let rec = Record {
        path: path.clone(),
        manifest: manifest.clone(),
        kind: Kind::Pixi,
        pixi_env: pixi_env.clone().unwrap_or_else(|| "default".into()),
        created: Some(now_iso()),
    };
    let env = Env::new(name, &rec);
    if let Some(pe) = &pixi_env {
        if pe != "default" && env.pixi_table().get("environments").and_then(|e| e.get(pe)).is_none() {
            warn(&format!("pixi environment '{}' is not defined in {} (yet)", pe, manifest.display()));
        }
    }
    update_registry(|r| {
        r.envs.insert(name.to_string(), rec.clone());
        Ok(())
    })?;
    runtime::write_shim_link(&env)?;
    runtime::write_activate_script(&env)?;
    env.write_env_file()?;

    if created && !packages.is_empty() {
        let ok = Command::new(find_pixi()?)
            .arg("add")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--frozen")
            .args(&packages)
            .status()?
            .success();
        if !ok {
            warn(&format!("adding packages failed; edit {} by hand", manifest.display()));
        }
    } else if !packages.is_empty() {
        warn("packages are only added to newly created manifests; use `cvenv add`");
    }
    info(&format!("created env '{}' in {}", name, path.display()));
    println!(
        "Next steps:\n    cvenv activate {}\n    cvenv add python numpy      # edits {} only\n    cvenv rebuild               # builds the image",
        name,
        manifest.display()
    );
    Ok(())
}

fn cmd_envs() -> Result<()> {
    let reg = load_registry();
    if reg.envs.is_empty() {
        println!("no envs registered (create one with `cvenv init NAME`). store: {}", config::store_dir().display());
        return Ok(());
    }
    let active = std::env::var("CVENV_ACTIVE").unwrap_or_default();
    let mut rows = vec![];
    for (n, rec) in &reg.envs {
        let e = Env::new(n, rec);
        let status = if let Some((job, state)) = build::slurm_build_state(&e) {
            format!("{} (job {})", if state == "PENDING" { "queued" } else { "building" }, job)
        } else if build::is_building(&e) {
            "building".to_string()
        } else if !e.manifest.is_file() {
            "manifest missing".to_string()
        } else if !e.built() {
            "not built".to_string()
        } else {
            let s = staleness(&e);
            if s.changed { "stale" } else if s.moved { "moved" } else { "ok" }.to_string()
        };
        let size = if e.built() { human_size(fs::metadata(e.image())?.len()) } else { "-".into() };
        rows.push((if *n == active { "*" } else { " " }, n.clone(), status, size, e.path.display().to_string()));
    }
    let w1 = rows.iter().map(|r| r.1.len()).max().unwrap_or(4).max(4);
    let w2 = rows.iter().map(|r| r.2.len()).max().unwrap_or(6).max(6);
    println!("  {:<w1$}  {:<w2$}  {:>7}  PATH", "NAME", "STATUS", "SIZE", w1 = w1, w2 = w2);
    for r in rows {
        println!("{} {:<w1$}  {:<w2$}  {:>7}  {}", r.0, r.1, r.2, r.3, r.4, w1 = w1, w2 = w2);
    }
    Ok(())
}

fn cmd_info(name: Option<&str>) -> Result<()> {
    let e = resolve_env(name, true, true)?;
    let meta = e.meta();
    println!("env         : {}{}{}", e.name, if e.is_active() { "  (active)" } else { "" }, if e.unregistered { "  (not in your registry)" } else { "" });
    println!("kind        : {}", e.kind.label());
    println!("path        : {}", e.path.display());
    println!("manifest    : {}", e.manifest.display());
    println!("pixi env    : {}", e.pixi_env);
    println!("image       : {}{}", e.image().display(), if e.built() { "" } else { "  (not built)" });
    if e.built() {
        println!("image size  : {}", human_size(fs::metadata(e.image())?.len()));
        if let Some(m) = &meta {
            println!("built at    : {} ({}s)", m.built_at, m.build_seconds);
            println!("base image  : {}", m.base_image);
        }
        let n = fs::read_dir(e.shim_dir()).map(|rd| rd.filter_map(|x| x.ok()).filter(|x| x.file_name() != SHIM_LINK).count()).unwrap_or(0);
        println!("commands    : {} shimmed in {}", n, e.shim_dir().display());
    }
    let gf = gpu_flags(&e);
    println!("gpu flags   : {}", if gf.is_empty() { "(none on this node)".to_string() } else { gf.join(" ") });
    let s = staleness(&e);
    println!("status      : {}", if s.reasons.is_empty() { "up to date" } else { "needs attention" });
    for r in s.reasons {
        println!("              - {}", r);
    }
    Ok(())
}

fn cmd_activate(name: Option<&str>) -> Result<()> {
    let e = resolve_env(name, false, true)?;
    if stdout_is_tty() {
        die(
            &format!(
                "`cvenv activate` must change your current shell. Either add\n    eval \"$(cvenv shell-init)\"\n\
                 to ~/.bashrc (then `cvenv activate {n}` just works), or run\n    eval \"$(cvenv activate {n})\"   or   source {a}/activate",
                n = e.selector(),
                a = e.art().display()
            ),
            2,
        );
    }
    if !e.unregistered {
        // keep the shim link pointing at this cvenv binary (best effort)
        let _ = runtime::write_shim_link(&e);
        let _ = runtime::write_activate_script(&e);
    }
    print!("{}", runtime::activation_code(&e));
    if e.built() {
        info(&format!("activated '{}'", e.name));
    }
    warn_if_stale(&e);
    Ok(())
}

fn cmd_deactivate() -> Result<()> {
    if stdout_is_tty() {
        die("run `eval \"$(cvenv deactivate)\"` or install the shell hook (`cvenv shell-init`)", 2);
    }
    if std::env::var("CVENV_ACTIVE").map(|s| s.is_empty()).unwrap_or(true) {
        info("no cvenv env is active");
    }
    print!("{}", runtime::deactivation_code());
    Ok(())
}

fn cmd_move(name: &str, dest_arg: &str) -> Result<()> {
    let e = resolve_env(Some(name), false, false)?;
    let mut dest = expand(dest_arg);
    if dest.is_dir() || dest_arg.ends_with('/') {
        dest = dest.join(e.path.file_name().unwrap_or_default());
    }
    if dest.exists() {
        bail!("destination {} already exists", dest.display());
    }
    if is_under(&dest, &e.path) {
        bail!("cannot move an env into itself");
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        // fail early if a build is running
        let _l = util::lock_file(&e.art().join(".build.lock"), false, &format!("env '{}' (a build is running)", e.name))?;
    }
    info(&format!("moving '{}': {} -> {}", e.name, e.path.display(), dest.display()));
    move_dir(&e.path, &dest)?;
    let old = e.path.clone();
    let mut new_rec = None;
    update_registry(|r| {
        let rec = r.envs.get_mut(name).ok_or_else(|| anyhow::anyhow!("env vanished from the registry"))?;
        rec.path = dest.clone();
        if is_under(&rec.manifest, &old) {
            rec.manifest = dest.join(rec.manifest.strip_prefix(&old).unwrap_or(Path::new("pixi.toml")));
        }
        new_rec = Some(rec.clone());
        Ok(())
    })?;
    let moved = Env::new(name, new_rec.as_ref().unwrap());
    runtime::write_shim_link(&moved)?;
    runtime::write_activate_script(&moved)?;
    moved.write_env_file()?;
    info(&format!("moved '{}' to {}", name, dest.display()));
    if e.manifest_inside() && moved.built() {
        info(&format!(
            "the image keeps working as-is; run `cvenv rebuild --env {}` if you use pixi tasks or editable/path dependencies",
            name
        ));
    }
    if moved.is_active() {
        info(&format!("other shells where it is active need `cvenv activate {}` again", name));
    }
    Ok(())
}

/// rename(2), falling back to copy + delete across filesystems.
fn move_dir(src: &Path, dst: &Path) -> Result<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    let ok = Command::new("cp").arg("-a").arg(src).arg(dst).status()?.success();
    if !ok {
        remove_tree(dst);
        bail!("copying {} to {} failed", src.display(), dst.display());
    }
    remove_tree(src);
    Ok(())
}

fn cmd_unregister(name: &str, delete: bool, yes: bool) -> Result<()> {
    let e = resolve_env(Some(name), false, false)?;
    if delete {
        let targets: Vec<PathBuf> = if e.manifest_inside() { vec![e.path.clone()] } else { vec![e.art()] };
        if !yes {
            if !stdin_is_tty() {
                bail!("refusing to delete without --yes");
            }
            eprint!("delete {} ? [y/N] ", targets.iter().map(|t| t.display().to_string()).collect::<Vec<_>>().join(", "));
            let mut ans = String::new();
            std::io::stdin().read_line(&mut ans)?;
            if !matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
                bail!("aborted");
            }
        }
        for t in targets {
            if t.exists() {
                remove_tree(&t);
                info(&format!("deleted {}", t.display()));
            }
        }
        // external manifest: drop the (now empty) artifacts directory too
        if !e.manifest_inside() {
            let _ = fs::remove_dir(&e.path);
        }
    }
    update_registry(|r| {
        r.envs.remove(name);
        Ok(())
    })?;
    info(&format!("unregistered '{}'", name));
    if e.is_active() {
        info("it is still active in this shell; run `cvenv deactivate`");
    }
    Ok(())
}

fn cmd_config(action: Option<&str>, key: Option<&str>, value: Option<&str>) -> Result<()> {
    match action {
        None | Some("list") | Some("show") => {
            println!("config file : {}", config::config_path().display());
            println!("registry    : {}", config::registry_path().display());
            for (k, _, desc) in config::KEYS {
                let (val, src) = match config::value_with_source(k) {
                    Some((v, config::Source::Env(e))) => (v, format!("env {}", e)),
                    Some((v, config::Source::Config)) => (v, "config".into()),
                    Some((v, config::Source::InstallConf)) => (v, "install.conf".into()),
                    None => (if *k == "store" { config::store_dir().display().to_string() } else { "(auto)".into() }, "default".into()),
                };
                println!("{:<14} = {:<50} [{}]  # {}", k, val, src, desc);
            }
            Ok(())
        }
        Some(a @ ("get" | "set" | "unset")) => {
            let key = key.ok_or_else(|| anyhow::anyhow!("usage: cvenv config {} KEY{}", a, if a == "set" { " VALUE" } else { "" }))?;
            if config::key_env(key).is_none() {
                bail!("unknown key '{}'. Keys: {}", key, config::KEYS.iter().map(|k| k.0).collect::<Vec<_>>().join(", "));
            }
            match a {
                "get" => {
                    let v = if key == "store" { Some(config::store_dir().display().to_string()) } else { config::value(key) };
                    println!("{}", v.unwrap_or_default());
                }
                "set" => {
                    let v = value.ok_or_else(|| anyhow::anyhow!("usage: cvenv config set KEY VALUE"))?;
                    let v = if config::PATH_KEYS.contains(&key) { expand(v).display().to_string() } else { v.to_string() };
                    config::set_value(key, Some(&v))?;
                    info(&format!("{} = {}", key, v));
                    if key == "store" {
                        info(&format!("new envs will be created in {} (existing envs stay put; use `cvenv move`)", v));
                    }
                }
                _ => {
                    config::set_value(key, None)?;
                    info(&format!("unset {}", key));
                }
            }
            Ok(())
        }
        Some(other) => bail!("unknown config action '{}' (list, get, set, unset)", other),
    }
}

// ---------------------------------------------------------------------------
// pass-through commands
// ---------------------------------------------------------------------------

/// Extract `--env NAME` / `-n NAME` / `--env=NAME`. With `stop_at_positional`, only
/// options before the first positional argument are examined (for `run CMD ...`).
fn split_env_opt(argv: &[String], stop_at_positional: bool) -> Result<(Option<String>, Vec<String>)> {
    let mut name = None;
    let mut rest = vec![];
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            let from = if stop_at_positional { i + 1 } else { i };
            rest.extend(argv[from..].iter().cloned());
            break;
        }
        if a == "--env" || a == "-n" {
            name = Some(argv.get(i + 1).ok_or_else(|| anyhow::anyhow!("{} needs a value", a))?.clone());
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--env=") {
            name = Some(v.to_string());
            i += 1;
            continue;
        }
        if stop_at_positional && !a.starts_with('-') {
            rest.extend(argv[i..].iter().cloned());
            break;
        }
        rest.push(a.clone());
        i += 1;
    }
    Ok((name, rest))
}

fn host_pixi(args: &[String]) -> Result<bool> {
    let mut cmd = Command::new(find_pixi()?);
    cmd.args(args);
    for k in runtime::HOST_ENV_DROP {
        cmd.env_remove(k);
    }
    Ok(cmd.status()?.success())
}

fn cmd_add_remove(verb: &str, argv: &[String]) -> Result<()> {
    let (name, rest) = split_env_opt(argv, false)?;
    if rest.is_empty() || rest[0] == "-h" || rest[0] == "--help" {
        println!(
            "usage: cvenv {v} [--env NAME] [pixi {v} options] SPEC...\n\
             Edits the env's manifest only (`pixi {v} --frozen`); run `cvenv rebuild` to apply.\n\
             Examples: cvenv {v} numpy 'scipy>=1.11'   cvenv {v} --pypi requests   cvenv {v} --feature cuda pytorch-gpu",
            v = verb
        );
        return Ok(());
    }
    let e = resolve_env(name.as_deref(), true, true)?;
    let mut args = vec![verb.to_string(), "--manifest-path".into(), e.manifest.display().to_string(), "--frozen".into()];
    args.extend(rest);
    if !host_pixi(&args)? {
        std::process::exit(1);
    }
    info(&format!(
        "{} updated; the image is unchanged until you run `{}`",
        e.manifest.file_name().unwrap_or_default().to_string_lossy(),
        e.rebuild_hint()
    ));
    Ok(())
}

fn cmd_task(argv: &[String]) -> Result<()> {
    let (name, mut rest) = split_env_opt(argv, false)?;
    let e = resolve_env(name.as_deref(), true, true)?;
    if rest.is_empty() {
        rest.push("list".into());
    }
    let mut args = vec!["task".to_string(), rest[0].clone(), "--manifest-path".into(), e.manifest.display().to_string()];
    args.extend(rest[1..].iter().cloned());
    if !host_pixi(&args)? {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_list(argv: &[String]) -> Result<()> {
    let (name, rest) = split_env_opt(argv, false)?;
    let e = resolve_env(name.as_deref(), true, true)?;
    let p = e.art().join("packages.txt");
    let text = fs::read_to_string(&p).map_err(|_| anyhow::anyhow!("env '{}' has not been built yet", e.name))?;
    let pats: Vec<&String> = rest.iter().filter(|a| !a.starts_with('-')).collect();
    for (i, line) in text.lines().enumerate() {
        if i == 0 || pats.is_empty() || pats.iter().any(|p| line.contains(p.as_str())) {
            println!("{}", line);
        }
    }
    warn_if_stale(&e);
    Ok(())
}

fn cmd_run(argv: &[String], exec_mode: bool) -> Result<()> {
    let (name, mut rest) = split_env_opt(argv, true)?;
    if rest.first().map(|a| a == "--").unwrap_or(false) {
        rest.remove(0);
    }
    if rest.is_empty() {
        bail!("usage: cvenv run [--env NAME] COMMAND [ARGS...]   (COMMAND may be a pixi task)");
    }
    let e = resolve_env(name.as_deref(), true, true)?;
    if !e.built() {
        bail!("env '{}' has not been built yet - run `{}`", e.name, e.rebuild_hint());
    }
    warn_if_stale(&e);
    if !exec_mode && env::task_names(&e).contains(&rest[0]) {
        let mut argv = vec![
            IMG_PIXI.to_string(),
            "run".into(),
            "--frozen".into(),
            "--manifest-path".into(),
            e.manifest.display().to_string(),
            "-e".into(),
            e.pixi_env.clone(),
        ];
        argv.extend(rest);
        rest = argv;
        // pixi re-checks [system-requirements] cuda; CPU-only nodes have no __cuda
        if std::env::var_os("CONDA_OVERRIDE_CUDA").is_none() {
            if let Some(c) = env::cuda_requirement(&e) {
                std::env::set_var("CONDA_OVERRIDE_CUDA", c);
            }
        }
        // pixi may touch its repodata cache: keep it on node-local disk, not $HOME
        if std::env::var_os("PIXI_CACHE_DIR").is_none() {
            std::env::set_var("PIXI_CACHE_DIR", build::build_tmp_root()?.join("pixi-run-cache"));
        }
    }
    exec_in_container(&e, &rest, "exec")
}

fn cmd_shell(argv: &[String]) -> Result<()> {
    let (name, rest) = split_env_opt(argv, true)?;
    let e = resolve_env(name.as_deref(), true, true)?;
    if !e.built() {
        bail!("env '{}' has not been built yet - run `{}`", e.name, e.rebuild_hint());
    }
    warn_if_stale(&e);
    exec_in_container(&e, &rest, "shell")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn env_option_parsing() {
        let (n, r) = split_env_opt(&s(&["--pypi", "--env", "foo", "requests"]), false).unwrap();
        assert_eq!(n.as_deref(), Some("foo"));
        assert_eq!(r, s(&["--pypi", "requests"]));
        // for `run`, options after the command belong to the command
        let (n, r) = split_env_opt(&s(&["-n", "foo", "python", "--env", "x"]), true).unwrap();
        assert_eq!(n.as_deref(), Some("foo"));
        assert_eq!(r, s(&["python", "--env", "x"]));
        let (n, r) = split_env_opt(&s(&["--env=bar", "--", "-c", "1"]), true).unwrap();
        assert_eq!(n.as_deref(), Some("bar"));
        assert_eq!(r, s(&["-c", "1"]));
    }

    #[test]
    fn pip_detection() {
        assert!(is_pip("pip") && is_pip("pip3") && is_pip("pip3.12"));
        assert!(!is_pip("pipx") && !is_pip("pip3.x"));
    }
}
