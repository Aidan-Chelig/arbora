use crate::{
    cache,
    config::{self, LOCK},
    merkle,
    store::{LocalStore, ObjectInfo, ObjectStore},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub struct Analysis {
    pub retained_roots: BTreeSet<String>,
    pub retained_objects: BTreeSet<String>,
    pub candidates: Vec<ObjectInfo>,
}

pub fn analyze(
    roots: BTreeSet<String>,
    remote: &dyn ObjectStore,
    cache_store: &LocalStore,
    concurrency: usize,
    older_than_seconds: Option<u64>,
) -> Result<Analysis> {
    ensure!(
        !roots.is_empty(),
        "remote GC requires at least one retained root"
    );
    let mut retained_objects = BTreeSet::new();
    for root in &roots {
        crate::store::object_key(root, "")
            .with_context(|| format!("invalid retained root {root:?}; remote GC aborted"))?;
        cache::fetch_tree(root, remote, cache_store, concurrency)
            .with_context(|| format!("verify retained root {root}; remote GC aborted"))?;
        retained_objects.extend(
            merkle::reachable(cache_store, root)
                .with_context(|| format!("traverse retained root {root}; remote GC aborted"))?,
        );
    }

    let cutoff = older_than_seconds.map(|age| {
        unix_now()
            .unwrap_or_default()
            .saturating_sub(i64::try_from(age).unwrap_or(i64::MAX))
    });
    let mut candidates = remote
        .list_objects()?
        .into_iter()
        .filter(|object| !retained_objects.contains(&object.hash))
        .filter(|object| match cutoff {
            None => true,
            Some(cutoff) => object
                .modified_unix_seconds
                .is_some_and(|modified| modified <= cutoff),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.hash.cmp(&right.hash));
    Ok(Analysis {
        retained_roots: roots,
        retained_objects,
        candidates,
    })
}

pub fn parse_age(value: &str) -> Result<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, suffix) = value.split_at(split);
    ensure!(!amount.is_empty(), "invalid age {value:?}");
    let amount: u64 = amount
        .parse()
        .with_context(|| format!("invalid age {value:?}"))?;
    let unit = match suffix {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => bail!("invalid age {value:?}; use s, m, h, d, or w (for example 90d)"),
    };
    amount.checked_mul(unit).context("age is too large")
}

pub fn roots_from_git(
    project: &Path,
    all_reachable: bool,
    keep_last: Option<usize>,
) -> Result<BTreeSet<String>> {
    let top = git_output(project, ["rev-parse", "--show-toplevel"])
        .context("--roots-from-git requires a Git worktree")?;
    let top = PathBuf::from(top.trim());
    let relative_project = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_owned())
        .strip_prefix(top.canonicalize().unwrap_or_else(|_| top.clone()))
        .context("project is outside its Git worktree")?
        .to_owned();
    let lock_path = relative_project
        .join(LOCK)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");

    let limit = if all_reachable {
        None
    } else {
        let limit = keep_last.context("Git root discovery requires a retention policy")?;
        ensure!(limit > 0, "--keep-last must be greater than zero");
        Some(limit)
    };
    let revisions = if all_reachable {
        git_output(project, ["rev-list", "--all", "--date-order"])?
    } else {
        git_output(project, ["log", "--all", "--format=%H", "--date-order"])?
    };

    let mut roots = BTreeSet::new();
    for revision in revisions.lines().filter(|line| !line.is_empty()) {
        let specification = format!("{revision}:{lock_path}");
        let exists = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(["cat-file", "-e", &specification])
            .output()
            .with_context(|| format!("run git cat-file -e {specification}"))?;
        if !exists.status.success() {
            // A commit predating Arbora simply has no retained lock.
            continue;
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(["show", &specification])
            .output()
            .with_context(|| format!("run git show {specification}"))?;
        ensure!(
            output.status.success(),
            "git show {specification} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let text = String::from_utf8(output.stdout)
            .with_context(|| format!("{specification} is not UTF-8"))?;
        roots.insert(
            config::parse_lock(&text)
                .with_context(|| format!("parse {specification}"))?
                .root,
        );
        if limit.is_some_and(|limit| roots.len() >= limit) {
            break;
        }
    }
    Ok(roots)
}

fn git_output<const N: usize>(project: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .context("run git")?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git output is not UTF-8")
}

pub fn unix_now() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?)
}

pub fn unix_now_nanos() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        merkle::{Entry, Kind, Tree, blob_object, encode_tree, hash_object},
        store::ObjectStore,
    };

    #[test]
    fn ages_are_bounded_and_explicit() {
        assert_eq!(parse_age("90d").unwrap(), 7_776_000);
        assert_eq!(parse_age("2w").unwrap(), 1_209_600);
        assert!(parse_age("90").is_err());
        assert!(parse_age("days").is_err());
    }

    #[test]
    fn analysis_keeps_every_object_reachable_from_retained_roots() {
        let temp = tempfile::tempdir().unwrap();
        let remote = LocalStore::new(temp.path().join("remote"));
        let cache_store = LocalStore::new(temp.path().join("cache"));
        let kept_blob = blob_object(b"keep");
        let kept_hash = hash_object(&kept_blob);
        remote.put(&kept_hash, &kept_blob).unwrap();
        let tree = Tree::from([(
            "kept.bin".into(),
            Entry {
                kind: Kind::Blob,
                hash: kept_hash.clone(),
                executable: false,
            },
        )]);
        let tree_object = encode_tree(&tree).unwrap();
        let root = hash_object(&tree_object);
        remote.put(&root, &tree_object).unwrap();
        let garbage = blob_object(b"garbage");
        let garbage_hash = hash_object(&garbage);
        remote.put(&garbage_hash, &garbage).unwrap();

        let analysis = analyze(
            BTreeSet::from([root.clone()]),
            &remote,
            &cache_store,
            2,
            None,
        )
        .unwrap();
        assert_eq!(analysis.retained_objects, BTreeSet::from([root, kept_hash]));
        assert_eq!(analysis.candidates.len(), 1);
        assert_eq!(analysis.candidates[0].hash, garbage_hash);
    }
}
