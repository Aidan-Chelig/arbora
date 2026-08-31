use crate::{
    cache,
    config::{self, CONFIG, Config},
    merkle,
    store::{HttpStore, LocalStore, ObjectStore, S3Options, S3Store},
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(version, about = "Content-addressed asset synchronization")]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    project: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Initialize configuration and the asset workspace.
    Init {
        #[arg(long, default_value = "assets")]
        workspace: PathBuf,
        #[arg(long, default_value = ".arbora-remote")]
        remote: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Compare the workspace to assets.lock.
    Status,
    /// Store the workspace in the configured remote and update assets.lock.
    Push,
    /// Materialize the tree named by assets.lock.
    Pull {
        #[arg(long)]
        keep_stale: bool,
    },
    /// Pull when clean; push when changed. Use --pull to discard local changes.
    Sync {
        #[arg(long)]
        pull: bool,
    },
    /// Verify all objects reachable from assets.lock and the workspace contents.
    Verify,
    /// Remove cache objects not reachable from this project's lock.
    Gc,
    /// Show file changes against assets.lock, or between two root hashes.
    Diff {
        from: Option<String>,
        to: Option<String>,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let project = absolute(&cli.project)?;
    match cli.command {
        Command::Init {
            workspace,
            remote,
            force,
        } => init(&project, workspace, remote, force),
        Command::Status => status(&project),
        Command::Push => push(&project),
        Command::Pull { keep_stale } => pull(&project, keep_stale),
        Command::Sync { pull: discard } => sync(&project, discard),
        Command::Verify => verify(&project),
        Command::Gc => gc(&project),
        Command::Diff { from, to } => diff(&project, from, to),
    }
}
fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_owned())
    } else {
        Ok(env::current_dir()?.join(p))
    }
}
fn stores(project: &Path) -> Result<(Config, Box<dyn ObjectStore>, LocalStore)> {
    let c = Config::load(project)?;
    let remote: Box<dyn ObjectStore> = match c.remote.kind.as_str() {
        "local" => Box::new(LocalStore::new(
            project.join(
                c.remote
                    .path
                    .as_ref()
                    .context("local remote requires `path`")?,
            ),
        )),
        "http" | "https" => Box::new(HttpStore::new(
            c.remote
                .url
                .as_deref()
                .context("HTTP remote requires `url`")?,
            &c.remote.prefix,
        )?),
        "s3" => Box::new(S3Store::new(S3Options {
            bucket: c
                .remote
                .bucket
                .clone()
                .context("S3 remote requires `bucket`")?,
            prefix: c.remote.prefix.clone(),
            endpoint: c.remote.endpoint.clone(),
            region: c.remote.region.clone(),
            profile: c.remote.profile.clone(),
            access_key_id: c.remote.access_key_id.clone(),
            secret_access_key: c.remote.secret_access_key.clone(),
            session_token: c.remote.session_token.clone(),
            anonymous: c.remote.anonymous,
            force_path_style: c.remote.force_path_style,
        })?),
        other => bail!("unsupported remote type {other:?}; expected local, http, or s3"),
    };
    let cache = LocalStore::new(c.cache()?);
    Ok((c, remote, cache))
}
fn init(project: &Path, workspace: PathBuf, remote: PathBuf, force: bool) -> Result<()> {
    fs::create_dir_all(project)?;
    let path = project.join(CONFIG);
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to replace it)",
            path.display()
        )
    }
    let c = Config {
        remote: config::Remote {
            kind: "local".into(),
            path: Some(remote.clone()),
            url: None,
            bucket: None,
            prefix: String::new(),
            endpoint: None,
            region: None,
            profile: None,
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
            anonymous: false,
            force_path_style: false,
        },
        workspace: config::Workspace {
            path: workspace.clone(),
            remove_stale: true,
        },
        cache: config::Cache::default(),
    };
    fs::write(&path, toml::to_string_pretty(&c)?)?;
    fs::create_dir_all(project.join(workspace))?;
    fs::create_dir_all(project.join(remote).join("objects"))?;
    println!("initialized {}", project.display());
    Ok(())
}
fn workspace_root(project: &Path, c: &Config, cache: &LocalStore) -> Result<String> {
    Ok(merkle::scan(&c.workspace(project), &[cache])?.0)
}
fn status(project: &Path) -> Result<()> {
    let (c, _, cache) = stores(project)?;
    let current = workspace_root(project, &c, &cache)?;
    let expected = config::read_lock(project)?.root;
    if current == expected {
        println!("clean\nroot {current}")
    } else {
        println!("modified\nexpected {expected}\nactual   {current}");
        show_diff(&cache, &expected, &current)?;
    }
    Ok(())
}
fn push(project: &Path) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let (root, stats) = merkle::scan(&c.workspace(project), &[&cache, remote.as_ref()])?;
    config::write_lock(project, &root)?;
    println!(
        "pushed {root}\n{} blobs, {} trees, {} bytes",
        stats.blobs, stats.trees, stats.bytes
    );
    Ok(())
}
fn pull(project: &Path, keep_stale: bool) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let root = config::read_lock(project)?.root;
    let fetched = cache::fetch_tree(&root, remote.as_ref(), &cache)?;
    cache::materialize(
        &root,
        &c.workspace(project),
        &cache,
        c.workspace.remove_stale && !keep_stale,
    )?;
    println!("pulled {root}\n{fetched} objects fetched");
    Ok(())
}
fn sync(project: &Path, discard: bool) -> Result<()> {
    let (c, _, cache) = stores(project)?;
    let lock = config::read_lock(project)?.root;
    let current = workspace_root(project, &c, &cache)?;
    if current == lock || discard {
        pull(project, false)
    } else {
        push(project)
    }
}
fn verify(project: &Path) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let root = config::read_lock(project)?.root;
    let remote_objects = merkle::reachable(remote.as_ref(), &root).context("verify remote")?;
    cache::fetch_tree(&root, remote.as_ref(), &cache)?;
    let current = workspace_root(project, &c, &cache)?;
    if current != root {
        bail!("workspace root {current} does not match locked root {root}")
    }
    println!(
        "verified {} objects and workspace root {root}",
        remote_objects.len()
    );
    Ok(())
}
fn gc(project: &Path) -> Result<()> {
    let (_, remote, cache) = stores(project)?;
    let root = config::read_lock(project)?.root;
    cache::fetch_tree(&root, remote.as_ref(), &cache)?;
    let keep = merkle::reachable(&cache, &root)?;
    let (count, bytes) = cache::gc(&cache, &keep)?;
    println!(
        "removed {count} objects ({bytes} bytes); kept {}",
        keep.len()
    );
    Ok(())
}
fn diff(project: &Path, from: Option<String>, to: Option<String>) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let default = config::read_lock(project)?.root;
    let from = from.unwrap_or(default);
    cache::fetch_tree(&from, remote.as_ref(), &cache)?;
    let to = match to {
        Some(h) => {
            cache::fetch_tree(&h, remote.as_ref(), &cache)?;
            h
        }
        None => workspace_root(project, &c, &cache)?,
    };
    show_diff(&cache, &from, &to)
}
fn show_diff(store: &LocalStore, from: &str, to: &str) -> Result<()> {
    if from == to {
        return Ok(());
    }
    let a = merkle::flatten(store, from)?;
    let b = merkle::flatten(store, to)?;
    let paths: BTreeSet<_> = a.keys().chain(b.keys()).collect();
    for path in paths {
        match (a.get(path), b.get(path)) {
            (None, Some(_)) => println!("A {}", path.display()),
            (Some(_), None) => println!("D {}", path.display()),
            (Some(x), Some(y)) if x != y => println!("M {}", path.display()),
            _ => {}
        }
    }
    Ok(())
}
