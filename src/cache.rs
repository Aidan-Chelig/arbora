use crate::{
    merkle::{Kind, decode_blob, decode_tree, verify_object},
    store::{LocalStore, ObjectStore},
};
use anyhow::Result;
use ignore::gitignore::Gitignore;
use std::{collections::BTreeSet, fs, path::Path};

pub fn copy_object(hash: &str, from: &dyn ObjectStore, to: &dyn ObjectStore) -> Result<bool> {
    if to.exists(hash)? {
        return Ok(false);
    }
    let bytes = verify_object(from, hash)?;
    to.put(hash, &bytes)
}
pub fn fetch_tree(root: &str, remote: &dyn ObjectStore, cache: &dyn ObjectStore) -> Result<usize> {
    fn visit(hash: &str, r: &dyn ObjectStore, c: &dyn ObjectStore, n: &mut usize) -> Result<()> {
        if !c.exists(hash)? && crate::cache::copy_object(hash, r, c)? {
            *n += 1
        }
        let bytes = verify_object(c, hash)?;
        if let Ok(tree) = decode_tree(&bytes) {
            for e in tree.values() {
                visit(&e.hash, r, c, n)?
            }
        } else {
            decode_blob(&bytes)?;
        }
        Ok(())
    }
    let mut n = 0;
    visit(root, remote, cache, &mut n)?;
    Ok(n)
}
pub fn materialize(
    root: &str,
    workspace: &Path,
    cache: &dyn ObjectStore,
    remove_stale: bool,
    ignore: Option<&Gitignore>,
) -> Result<()> {
    let staging = workspace.with_extension(format!("arbora-staging-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?
    }
    fs::create_dir_all(&staging)?;
    fn tree(hash: &str, dir: &Path, s: &dyn ObjectStore) -> Result<()> {
        for (name, e) in decode_tree(&verify_object(s, hash)?)? {
            let path = dir.join(name);
            match e.kind {
                Kind::Tree => {
                    fs::create_dir(&path)?;
                    tree(&e.hash, &path, s)?
                }
                Kind::Blob => {
                    let bytes = verify_object(s, &e.hash)?;
                    fs::write(&path, decode_blob(&bytes)?)?;
                    set_exec(&path, e.executable)?
                }
            }
        }
        Ok(())
    }
    if let Err(e) = tree(root, &staging, cache) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    if workspace.exists() {
        if remove_stale {
            if let Some(ignore) = ignore {
                preserve_ignored(workspace, workspace, &staging, ignore)?;
            }
            let old = workspace.with_extension(format!("arbora-old-{}", std::process::id()));
            if old.exists() {
                fs::remove_dir_all(&old)?
            }
            fs::rename(workspace, &old)?;
            match fs::rename(&staging, workspace) {
                Ok(()) => fs::remove_dir_all(old)?,
                Err(e) => {
                    let _ = fs::rename(old, workspace);
                    return Err(e.into());
                }
            }
        } else {
            merge(&staging, workspace)?;
            fs::remove_dir_all(staging)?
        }
    } else {
        fs::rename(staging, workspace)?
    }
    Ok(())
}
fn preserve_ignored(
    root: &Path,
    source: &Path,
    destination: &Path,
    ignore: &Gitignore,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        let relative = source_path.strip_prefix(root)?;
        if ignore.matched(relative, file_type.is_dir()).is_ignore() {
            copy_path(&source_path, &destination_path)?;
        } else if file_type.is_dir() {
            preserve_ignored(root, &source_path, &destination_path, ignore)?;
        }
    }
    Ok(())
}
fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    if destination.is_dir() {
        fs::remove_dir_all(destination)?;
    } else if destination.exists() {
        fs::remove_file(destination)?;
    }
    copy_path_inner(source, destination)
}
fn copy_path_inner(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "ignored symbolic links cannot be preserved: {}",
            source.display()
        );
    }
    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_path_inner(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}
fn merge(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            merge(&e.path(), &to)?
        } else {
            fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}
#[cfg(unix)]
fn set_exec(path: &Path, yes: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(path)?.permissions();
    let m = p.mode();
    p.set_mode(if yes { m | 0o111 } else { m & !0o111 });
    fs::set_permissions(path, p)?;
    Ok(())
}
#[cfg(not(unix))]
fn set_exec(_: &Path, _: bool) -> Result<()> {
    Ok(())
}
pub fn gc(store: &LocalStore, keep: &BTreeSet<String>) -> Result<(usize, u64)> {
    let base = store.root().join("objects");
    if !base.exists() {
        return Ok((0, 0));
    }
    let mut count = 0;
    let mut bytes = 0;
    for p in walkdir::WalkDir::new(&base).min_depth(2).max_depth(2) {
        let p = p?;
        if !p.file_type().is_file() {
            continue;
        }
        let rel = p.path().strip_prefix(&base)?;
        let mut it = rel.iter();
        let hash = format!(
            "blake3:{}{}",
            it.next().unwrap().to_string_lossy(),
            it.next().unwrap().to_string_lossy()
        );
        if !keep.contains(&hash) {
            let m = p.metadata()?;
            fs::remove_file(p.path())?;
            count += 1;
            bytes += m.len();
        }
    }
    Ok((count, bytes))
}
