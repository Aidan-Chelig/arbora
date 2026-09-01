use crate::{
    merkle::{
        Kind, blob_prefix, decode_tree, verify_blob_content_file, verify_object, verify_object_file,
    },
    store::{LocalStore, ObjectStore},
};
use anyhow::{Context, Result, bail};
use ignore::gitignore::Gitignore;
use std::{collections::BTreeSet, fs, io::Read, path::Path};

const REFS: &str = "refs";
const PROJECTS: &str = "projects";
const LEGACY: &str = "legacy.keep";
const INITIALIZING: &str = "initializing";

pub fn initialize_references(store: &LocalStore) -> Result<()> {
    fs::create_dir_all(store.root())?;
    let refs = store.root().join(REFS);
    match fs::create_dir(&refs) {
        Ok(()) => {
            fs::write(
                refs.join(INITIALIZING),
                b"cache reference migration in progress\n",
            )?;
            let legacy = list_objects(store)?;
            let mut contents = String::new();
            for hash in legacy {
                contents.push_str(&hash);
                contents.push('\n');
            }
            fs::write(refs.join(LEGACY), contents)?;
            fs::create_dir(refs.join(PROJECTS))?;
            fs::remove_file(refs.join(INITIALIZING))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if refs.join(INITIALIZING).exists() {
                bail!(
                    "cache reference migration is incomplete at {}; remove the cache or finish the migration before running GC",
                    refs.display()
                )
            }
            fs::create_dir_all(refs.join(PROJECTS))?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub fn register_root(store: &LocalStore, project: &Path, root: &str) -> Result<()> {
    initialize_references(store)?;
    // The project path is an identity only; no project metadata or credentials
    // are written into the shared cache.
    let identity = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_owned())
        .to_string_lossy()
        .into_owned();
    let name = blake3::hash(identity.as_bytes()).to_hex().to_string();
    let projects = store.root().join(REFS).join(PROJECTS);
    let path = projects.join(&name);
    let temporary = projects.join(format!(".{name}-{}", std::process::id()));
    fs::write(&temporary, format!("{root}\n"))?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn referenced_objects(store: &LocalStore) -> Result<BTreeSet<String>> {
    initialize_references(store)?;
    let refs = store.root().join(REFS);
    let mut keep = BTreeSet::new();
    let legacy = refs.join(LEGACY);
    if legacy.exists() {
        for line in fs::read_to_string(&legacy)?.lines() {
            if !line.is_empty() {
                keep.insert(line.to_owned());
            }
        }
    }
    for entry in fs::read_dir(refs.join(PROJECTS))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let root = fs::read_to_string(entry.path())?.trim().to_owned();
        if root.is_empty() {
            continue;
        }
        let reachable = crate::merkle::reachable(store, &root).with_context(|| {
            format!(
                "resolve cache reference {} at root {root}",
                entry.path().display()
            )
        })?;
        keep.extend(reachable);
    }
    Ok(keep)
}

fn list_objects(store: &LocalStore) -> Result<BTreeSet<String>> {
    let base = store.root().join("objects");
    let mut objects = BTreeSet::new();
    if !base.exists() {
        return Ok(objects);
    }
    for entry in walkdir::WalkDir::new(&base).min_depth(2).max_depth(2) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&base)?;
        let mut parts = relative.iter();
        let Some(prefix) = parts.next() else { continue };
        let Some(remainder) = parts.next() else {
            continue;
        };
        let hash = format!(
            "blake3:{}{}",
            prefix.to_string_lossy(),
            remainder.to_string_lossy()
        );
        if store.object_path(&hash).is_ok() {
            objects.insert(hash);
        }
    }
    Ok(objects)
}

pub fn copy_object(hash: &str, from: &dyn ObjectStore, to: &LocalStore) -> Result<bool> {
    if to.exists(hash)? {
        return Ok(false);
    }
    let destination = to.object_path(hash)?;
    fs::create_dir_all(destination.parent().unwrap())?;
    let temporary = destination.parent().unwrap().join(format!(
        ".download-{}-{}",
        std::process::id(),
        &hash[7..15]
    ));
    if let Err(error) = from.download_to(hash, &temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = verify_object_file(&temporary, hash) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    to.install_file(hash, &temporary)
}
pub fn fetch_tree(
    root: &str,
    remote: &dyn ObjectStore,
    cache: &LocalStore,
    concurrency: usize,
) -> Result<usize> {
    anyhow::ensure!(concurrency > 0, "transfer concurrency must be positive");
    let mut pending = vec![(root.to_owned(), Kind::Tree)];
    let mut scheduled = BTreeSet::from([root.to_owned()]);
    let mut fetched = 0;
    while !pending.is_empty() {
        for chunk in pending.chunks(concurrency) {
            let results = std::thread::scope(|scope| {
                chunk
                    .iter()
                    .map(|(hash, _)| {
                        scope.spawn(move || {
                            if cache.exists(hash)? {
                                Ok(false)
                            } else {
                                copy_object(hash, remote, cache)
                            }
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().expect("transfer worker panicked"))
                    .collect::<Result<Vec<_>>>()
            })?;
            fetched += results
                .into_iter()
                .filter(|was_fetched| *was_fetched)
                .count();
        }
        let mut next = Vec::new();
        for (hash, kind) in pending {
            if kind == Kind::Tree {
                let bytes = verify_object(cache, &hash)?;
                for entry in decode_tree(&bytes)?.values() {
                    if scheduled.insert(entry.hash.clone()) {
                        next.push((entry.hash.clone(), entry.kind.clone()));
                    }
                }
            } else {
                verify_object_file(&cache.object_path(&hash)?, &hash)?;
            }
        }
        pending = next;
    }
    Ok(fetched)
}
pub fn upload_tree(
    root: &str,
    cache: &LocalStore,
    remote: &dyn ObjectStore,
    concurrency: usize,
) -> Result<usize> {
    anyhow::ensure!(concurrency > 0, "transfer concurrency must be positive");
    let objects = crate::merkle::reachable(cache, root)?
        .into_iter()
        .collect::<Vec<_>>();
    let mut uploaded = 0;
    for chunk in objects.chunks(concurrency) {
        let results = std::thread::scope(|scope| {
            chunk
                .iter()
                .map(|hash| {
                    scope.spawn(move || {
                        let source = cache.object_path(hash)?;
                        verify_object_file(&source, hash)?;
                        remote.upload_from(hash, &source)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().expect("transfer worker panicked"))
                .collect::<Result<Vec<_>>>()
        })?;
        uploaded += results
            .into_iter()
            .filter(|was_uploaded| *was_uploaded)
            .count();
    }
    Ok(uploaded)
}
pub fn materialize(
    root: &str,
    workspace: &Path,
    cache: &LocalStore,
    remove_stale: bool,
    ignore: Option<&Gitignore>,
) -> Result<()> {
    let staging = workspace.with_extension(format!("arbora-staging-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?
    }
    fs::create_dir_all(&staging)?;
    fn tree(hash: &str, dir: &Path, s: &LocalStore) -> Result<()> {
        for (name, e) in decode_tree(&verify_object(s, hash)?)? {
            let path = dir.join(name);
            match e.kind {
                Kind::Tree => {
                    fs::create_dir(&path)?;
                    tree(&e.hash, &path, s)?
                }
                Kind::Blob => {
                    let source = materialized_blob(s, &e.hash, e.executable)?;
                    materialize_file(&source, &path)?;
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

fn materialized_blob(
    cache: &LocalStore,
    hash: &str,
    executable: bool,
) -> Result<std::path::PathBuf> {
    let hex = hash
        .strip_prefix("blake3:")
        .context("unsupported blob hash")?;
    let mode = if executable { "executable" } else { "regular" };
    let destination = cache
        .root()
        .join("materialized")
        .join(mode)
        .join(&hex[..2])
        .join(&hex[2..]);
    if destination.exists() {
        if verify_blob_content_file(&destination, hash).is_ok()
            && path_is_executable(&destination) == executable
        {
            return Ok(destination);
        }
        fs::remove_file(&destination)?;
    }
    fs::create_dir_all(destination.parent().unwrap())?;
    let temporary = destination.parent().unwrap().join(format!(
        ".materialize-{}-{}",
        std::process::id(),
        &hex[..8]
    ));
    let object = cache.object_path(hash)?;
    verify_object_file(&object, hash)?;
    let mut source = fs::File::open(object)?;
    let mut prefix = vec![0; blob_prefix().len()];
    source.read_exact(&mut prefix)?;
    anyhow::ensure!(prefix == blob_prefix(), "object {hash} is not a blob");
    let mut output = fs::File::create(&temporary)?;
    std::io::copy(&mut source, &mut output)?;
    drop(output);
    set_exec(&temporary, executable)?;
    match fs::rename(&temporary, &destination) {
        Ok(()) => {}
        Err(_) if destination.exists() => {
            let _ = fs::remove_file(temporary);
        }
        Err(error) => return Err(error.into()),
    }
    verify_blob_content_file(&destination, hash)?;
    Ok(destination)
}

#[cfg(unix)]
fn path_is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn path_is_executable(_path: &Path) -> bool {
    false
}

#[derive(Debug, Eq, PartialEq)]
enum MaterializationMethod {
    Reflink,
    Hardlink,
    Copy,
}

fn materialize_file(source: &Path, destination: &Path) -> Result<MaterializationMethod> {
    materialize_file_with(
        source,
        destination,
        |from, to| reflink_copy::reflink(from, to),
        |from, to| fs::hard_link(from, to),
    )
}

fn materialize_file_with(
    source: &Path,
    destination: &Path,
    reflink: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    hardlink: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<MaterializationMethod> {
    if reflink(source, destination).is_ok() {
        return Ok(MaterializationMethod::Reflink);
    }
    let _ = fs::remove_file(destination);
    if hardlink(source, destination).is_ok() {
        return Ok(MaterializationMethod::Hardlink);
    }
    let _ = fs::remove_file(destination);
    fs::copy(source, destination)?;
    Ok(MaterializationMethod::Copy)
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
            remove_materialized(store, &hash)?;
            count += 1;
            bytes += m.len();
        }
    }
    Ok((count, bytes))
}

fn remove_materialized(store: &LocalStore, hash: &str) -> Result<()> {
    let hex = hash.strip_prefix("blake3:").context("unsupported hash")?;
    for mode in ["regular", "executable"] {
        let path = store
            .root()
            .join("materialized")
            .join(mode)
            .join(&hex[..2])
            .join(&hex[2..]);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        merkle::{Entry, Tree, blob_object, encode_tree, hash_object},
        store::ObjectStore,
    };
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    struct TrackingStore {
        objects: BTreeMap<String, Vec<u8>>,
        active: AtomicUsize,
        maximum: AtomicUsize,
        uploads: Mutex<Vec<String>>,
    }
    impl TrackingStore {
        fn tracked<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(15));
            let result = action();
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }
    impl ObjectStore for TrackingStore {
        fn get(&self, hash: &str) -> Result<Vec<u8>> {
            Ok(self.objects.get(hash).unwrap().clone())
        }
        fn put(&self, _hash: &str, _bytes: &[u8]) -> Result<bool> {
            unreachable!()
        }
        fn exists(&self, hash: &str) -> Result<bool> {
            Ok(self.uploads.lock().unwrap().iter().any(|item| item == hash))
        }
        fn download_to(&self, hash: &str, destination: &Path) -> Result<()> {
            self.tracked(|| {
                fs::write(destination, self.objects.get(hash).unwrap())?;
                Ok(())
            })
        }
        fn upload_from(&self, hash: &str, _source: &Path) -> Result<bool> {
            self.tracked(|| {
                self.uploads.lock().unwrap().push(hash.to_owned());
                Ok(true)
            })
        }
    }

    #[test]
    fn transfer_parallelism_is_bounded() {
        let mut objects = BTreeMap::new();
        let mut tree = Tree::new();
        for index in 0..9 {
            let object = blob_object(format!("blob {index}").as_bytes());
            let hash = hash_object(&object);
            objects.insert(hash.clone(), object);
            tree.insert(
                format!("{index}.bin"),
                Entry {
                    kind: Kind::Blob,
                    hash,
                    executable: false,
                },
            );
        }
        let root_object = encode_tree(&tree).unwrap();
        let root = hash_object(&root_object);
        objects.insert(root.clone(), root_object);
        let remote = Arc::new(TrackingStore {
            objects,
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            uploads: Mutex::new(Vec::new()),
        });
        let temp = tempfile::tempdir().unwrap();
        let cache = LocalStore::new(temp.path());

        assert_eq!(fetch_tree(&root, remote.as_ref(), &cache, 3).unwrap(), 10);
        assert_eq!(remote.maximum.load(Ordering::SeqCst), 3);
        remote.maximum.store(0, Ordering::SeqCst);
        assert_eq!(upload_tree(&root, &cache, remote.as_ref(), 2).unwrap(), 10);
        assert_eq!(remote.maximum.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn materialization_falls_back_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"asset bytes").unwrap();

        let reflinked = temp.path().join("reflinked");
        let method = materialize_file_with(
            &source,
            &reflinked,
            |from, to| {
                fs::copy(from, to)?;
                Ok(())
            },
            |_, _| panic!("hardlink must not run after a reflink succeeds"),
        )
        .unwrap();
        assert_eq!(method, MaterializationMethod::Reflink);

        let hardlinked = temp.path().join("hardlinked");
        let method = materialize_file_with(
            &source,
            &hardlinked,
            |_, _| Err(std::io::Error::other("unsupported")),
            |from, to| fs::hard_link(from, to),
        )
        .unwrap();
        assert_eq!(method, MaterializationMethod::Hardlink);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&source).unwrap().ino(),
                fs::metadata(&hardlinked).unwrap().ino()
            );
        }

        let copied = temp.path().join("copied");
        let method = materialize_file_with(
            &source,
            &copied,
            |_, _| Err(std::io::Error::other("unsupported")),
            |_, _| Err(std::io::Error::other("cross-device")),
        )
        .unwrap();
        assert_eq!(method, MaterializationMethod::Copy);
        assert_eq!(fs::read(copied).unwrap(), b"asset bytes");
    }

    #[test]
    fn repairs_a_derived_blob_modified_through_a_hardlink() {
        let temp = tempfile::tempdir().unwrap();
        let cache = LocalStore::new(temp.path().join("cache"));
        let object = blob_object(b"canonical bytes");
        let hash = hash_object(&object);
        cache.put(&hash, &object).unwrap();
        let derived = materialized_blob(&cache, &hash, false).unwrap();
        let workspace = temp.path().join("workspace-file");
        fs::hard_link(&derived, &workspace).unwrap();
        fs::write(&workspace, b"locally edited").unwrap();

        let repaired = materialized_blob(&cache, &hash, false).unwrap();
        assert_eq!(fs::read(repaired).unwrap(), b"canonical bytes");
        assert_eq!(fs::read(workspace).unwrap(), b"locally edited");
    }
}
