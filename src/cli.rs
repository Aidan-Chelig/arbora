use crate::{
    cache,
    config::{self, CONFIG, Config},
    merkle, remote_gc,
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
    /// Remove unreferenced local-cache objects, or safely analyze a remote.
    Gc {
        /// Operate on the configured remote instead of the local cache.
        #[arg(long)]
        remote: bool,
        /// Explicitly select the default non-destructive mode.
        #[arg(long, conflicts_with = "confirm")]
        dry_run: bool,
        /// Delete the reported remote candidates.
        #[arg(long)]
        confirm: bool,
        /// Preserve an additional Arbora root (repeatable).
        #[arg(long, value_name = "ROOT")]
        keep_root: Vec<String>,
        /// Preserve locks from every commit reachable by any Git ref.
        #[arg(long)]
        roots_from_git: bool,
        /// Preserve locks from the most recent N Git commits.
        #[arg(long, value_name = "N")]
        keep_last: Option<usize>,
        /// Only collect unreferenced objects at least this old (for example 90d).
        #[arg(long, value_name = "AGE")]
        older_than: Option<String>,
        /// Write the GC report to this path instead of the cache report directory.
        #[arg(long, value_name = "PATH")]
        report: Option<PathBuf>,
    },
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
        Command::Gc {
            remote,
            dry_run,
            confirm,
            keep_root,
            roots_from_git,
            keep_last,
            older_than,
            report,
        } => gc(
            &project,
            RemoteGcOptions {
                remote,
                dry_run,
                confirm,
                keep_root,
                roots_from_git,
                keep_last,
                older_than,
                report,
            },
        ),
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
            retry_max_attempts: c.remote.retry_max_attempts,
            retry_max_backoff_ms: c.remote.retry_max_backoff_ms,
        })?),
        other => bail!("unsupported remote type {other:?}; expected local, http, or s3"),
    };
    let cache = LocalStore::new(c.cache()?);
    cache::initialize_references(&cache)?;
    Ok((c, remote, cache))
}
fn locked_root(project: &Path, cache: &LocalStore) -> Result<String> {
    let root = config::read_lock(project)?.root;
    cache::register_root(cache, project, &root)?;
    Ok(root)
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
            retry_max_attempts: 4,
            retry_max_backoff_ms: 5_000,
        },
        workspace: config::Workspace {
            path: workspace.clone(),
            remove_stale: true,
        },
        cache: config::Cache::default(),
        transfer: config::Transfer::default(),
    };
    fs::write(&path, toml::to_string_pretty(&c)?)?;
    let ignore = project.join(config::IGNORE);
    if !ignore.exists() {
        fs::write(
            ignore,
            "# Git-style patterns relative to the asset workspace.\n",
        )?;
    }
    fs::create_dir_all(project.join(workspace))?;
    fs::create_dir_all(project.join(remote).join("objects"))?;
    println!("initialized {}", project.display());
    Ok(())
}
fn workspace_root(project: &Path, c: &Config, cache: &LocalStore) -> Result<String> {
    let ignore = c.ignore(project)?;
    Ok(merkle::scan_with_ignore(&c.workspace(project), &[cache], Some(&ignore))?.0)
}
fn status(project: &Path) -> Result<()> {
    let (c, _, cache) = stores(project)?;
    let current = workspace_root(project, &c, &cache)?;
    let expected = locked_root(project, &cache)?;
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
    let ignore = c.ignore(project)?;
    let (root, stats) = merkle::scan_with_ignore(&c.workspace(project), &[&cache], Some(&ignore))?;
    let uploaded = cache::upload_tree(&root, &cache, remote.as_ref(), c.concurrency()?)?;
    config::write_lock(project, &root)?;
    cache::register_root(&cache, project, &root)?;
    println!(
        "pushed {root}\n{} blobs, {} trees, {} bytes, {} objects uploaded",
        stats.blobs, stats.trees, stats.bytes, uploaded
    );
    Ok(())
}
fn pull(project: &Path, keep_stale: bool) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let root = locked_root(project, &cache)?;
    let fetched = cache::fetch_tree(&root, remote.as_ref(), &cache, c.concurrency()?)?;
    let ignore = c.ignore(project)?;
    cache::materialize(
        &root,
        &c.workspace(project),
        &cache,
        c.workspace.remove_stale && !keep_stale,
        Some(&ignore),
    )?;
    println!("pulled {root}\n{fetched} objects fetched");
    Ok(())
}
fn sync(project: &Path, discard: bool) -> Result<()> {
    let (c, _, cache) = stores(project)?;
    let lock = locked_root(project, &cache)?;
    let current = workspace_root(project, &c, &cache)?;
    if current == lock || discard {
        pull(project, false)
    } else {
        push(project)
    }
}
fn verify(project: &Path) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let root = locked_root(project, &cache)?;
    cache::fetch_tree(&root, remote.as_ref(), &cache, c.concurrency()?)?;
    let remote_objects = merkle::reachable(&cache, &root)?;
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
struct RemoteGcOptions {
    remote: bool,
    dry_run: bool,
    confirm: bool,
    keep_root: Vec<String>,
    roots_from_git: bool,
    keep_last: Option<usize>,
    older_than: Option<String>,
    report: Option<PathBuf>,
}

fn gc(project: &Path, options: RemoteGcOptions) -> Result<()> {
    if options.remote {
        return remote_gc(project, options);
    }
    ensure_local_gc_options(&options)?;
    let (c, remote, cache) = stores(project)?;
    let root = locked_root(project, &cache)?;
    cache::fetch_tree(&root, remote.as_ref(), &cache, c.concurrency()?)?;
    let keep = cache::referenced_objects(&cache)?;
    let (count, bytes) = cache::gc(&cache, &keep)?;
    println!(
        "removed {count} objects ({bytes} bytes); kept {}",
        keep.len()
    );
    Ok(())
}

fn ensure_local_gc_options(options: &RemoteGcOptions) -> Result<()> {
    if options.dry_run
        || options.confirm
        || !options.keep_root.is_empty()
        || options.roots_from_git
        || options.keep_last.is_some()
        || options.older_than.is_some()
        || options.report.is_some()
    {
        bail!("remote retention options require arbora gc --remote");
    }
    Ok(())
}

fn remote_gc(project: &Path, options: RemoteGcOptions) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let mut roots = BTreeSet::from([locked_root(project, &cache)?]);
    roots.extend(options.keep_root);
    if options.roots_from_git {
        roots.extend(remote_gc::roots_from_git(project, true, None)?);
    }
    if let Some(keep_last) = options.keep_last {
        roots.extend(remote_gc::roots_from_git(project, false, Some(keep_last))?);
    }
    let older_than = options
        .older_than
        .as_deref()
        .map(remote_gc::parse_age)
        .transpose()?;
    let analysis =
        remote_gc::analyze(roots, remote.as_ref(), &cache, c.concurrency()?, older_than)?;
    let reclaimable_bytes = analysis
        .candidates
        .iter()
        .try_fold(0_u64, |total, object| total.checked_add(object.size))
        .context("reclaimable byte count overflow")?;
    let hashes = analysis
        .candidates
        .iter()
        .map(|object| object.hash.clone())
        .collect::<Vec<_>>();

    let action = if options.confirm {
        remote.delete_objects(&hashes)?;
        "deleted"
    } else {
        "would delete"
    };
    let report = write_remote_gc_report(
        options.report,
        project,
        &cache,
        action,
        &analysis,
        reclaimable_bytes,
    )?;
    println!(
        "remote GC {action} {} objects ({} bytes); retained {} roots and {} objects\nreport {}",
        hashes.len(),
        reclaimable_bytes,
        analysis.retained_roots.len(),
        analysis.retained_objects.len(),
        report.display()
    );
    if !options.confirm {
        println!("dry run; rerun with --confirm to delete these objects");
    }
    Ok(())
}

fn write_remote_gc_report(
    requested: Option<PathBuf>,
    project: &Path,
    cache: &LocalStore,
    action: &str,
    analysis: &remote_gc::Analysis,
    bytes: u64,
) -> Result<PathBuf> {
    let timestamp = remote_gc::unix_now()?;
    let report_id = remote_gc::unix_now_nanos()?;
    let path = match requested {
        Some(path) if path.is_relative() => project.join(path),
        Some(path) => path,
        None => cache
            .root()
            .join("gc-reports")
            .join(format!("remote-gc-{report_id}.txt")),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut report = format!(
        "arbora remote GC report\ntimestamp = {timestamp}\naction = {action}\nretained_roots = {}\nretained_objects = {}\ncandidate_objects = {}\ncandidate_bytes = {bytes}\n\nretained root hashes:\n",
        analysis.retained_roots.len(),
        analysis.retained_objects.len(),
        analysis.candidates.len(),
    );
    for root in &analysis.retained_roots {
        report.push_str(root);
        report.push('\n');
    }
    report.push_str("\nobject hashes:\n");
    for object in &analysis.candidates {
        report.push_str(&format!("{} {}\n", object.hash, object.size));
    }
    fs::write(&path, report)?;
    Ok(path)
}
fn diff(project: &Path, from: Option<String>, to: Option<String>) -> Result<()> {
    let (c, remote, cache) = stores(project)?;
    let default = locked_root(project, &cache)?;
    let from = from.unwrap_or(default);
    cache::fetch_tree(&from, remote.as_ref(), &cache, c.concurrency()?)?;
    let to = match to {
        Some(h) => {
            cache::fetch_tree(&h, remote.as_ref(), &cache, c.concurrency()?)?;
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
