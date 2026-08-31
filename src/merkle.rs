use crate::store::ObjectStore;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const BLOB: &[u8] = b"ARBORA\0BLOB\0";
const TREE: &[u8] = b"ARBORA\0TREE\0";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    Blob,
    Tree,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: Kind,
    pub hash: String,
    pub executable: bool,
}
pub type Tree = BTreeMap<String, Entry>;

pub fn hash_object(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}
pub fn blob_object(content: &[u8]) -> Vec<u8> {
    let mut out = BLOB.to_vec();
    out.extend_from_slice(content);
    out
}
pub fn decode_blob(bytes: &[u8]) -> Result<&[u8]> {
    bytes.strip_prefix(BLOB).context("object is not a blob")
}

pub fn encode_tree(tree: &Tree) -> Result<Vec<u8>> {
    let mut out = TREE.to_vec();
    for (name, entry) in tree {
        validate_name(name)?;
        ensure!(name.len() <= u32::MAX as usize, "file name too long");
        out.extend_from_slice(&(name.len() as u32).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(match entry.kind {
            Kind::Blob => 0,
            Kind::Tree => 1,
        });
        out.push(u8::from(entry.executable));
        let raw = entry
            .hash
            .strip_prefix("blake3:")
            .context("unsupported hash")?;
        let decoded = decode_hex(raw)?;
        ensure!(decoded.len() == 32, "invalid hash length");
        out.extend(decoded);
    }
    Ok(out)
}

pub fn decode_tree(bytes: &[u8]) -> Result<Tree> {
    let mut input = bytes.strip_prefix(TREE).context("object is not a tree")?;
    let mut tree = Tree::new();
    while !input.is_empty() {
        ensure!(input.len() >= 4, "truncated tree object");
        let len = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
        input = &input[4..];
        ensure!(input.len() >= len + 34, "truncated tree entry");
        let name = std::str::from_utf8(&input[..len])
            .context("non-UTF-8 tree name")?
            .to_owned();
        input = &input[len..];
        validate_name(&name)?;
        let kind = match input[0] {
            0 => Kind::Blob,
            1 => Kind::Tree,
            _ => bail!("invalid object kind"),
        };
        let executable = match input[1] {
            0 => false,
            1 => true,
            _ => bail!("invalid executable flag"),
        };
        let hash = format!("blake3:{}", encode_hex(&input[2..34]));
        input = &input[34..];
        ensure!(
            tree.insert(
                name,
                Entry {
                    kind,
                    hash,
                    executable
                }
            )
            .is_none(),
            "duplicate tree entry"
        );
    }
    Ok(tree)
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']),
        "unsafe tree entry name: {name:?}"
    );
    Ok(())
}
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn decode_hex(s: &str) -> Result<Vec<u8>> {
    ensure!(
        s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid hex hash"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

#[derive(Default, Debug)]
pub struct ScanStats {
    pub blobs: usize,
    pub trees: usize,
    pub bytes: u64,
}
pub fn scan(root: &Path, stores: &[&dyn ObjectStore]) -> Result<(String, ScanStats)> {
    ensure!(
        root.is_dir(),
        "workspace does not exist or is not a directory: {}",
        root.display()
    );
    let mut stats = ScanStats::default();
    let hash = scan_dir(root, stores, &mut stats)?;
    Ok((hash, stats))
}
fn scan_dir(dir: &Path, stores: &[&dyn ObjectStore], stats: &mut ScanStats) -> Result<String> {
    let mut paths: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    paths.sort_by_key(|e| e.file_name());
    let mut tree = Tree::new();
    for item in paths {
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF-8 file name in {}", dir.display()))?;
        validate_name(&name)?;
        let path = item.path();
        let meta = fs::symlink_metadata(&path)?;
        let (kind, hash, executable) = if meta.file_type().is_symlink() {
            bail!("symbolic links are not supported: {}", path.display())
        } else if meta.is_dir() {
            (Kind::Tree, scan_dir(&path, stores, stats)?, false)
        } else if meta.is_file() {
            let content = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            stats.bytes += content.len() as u64;
            let object = blob_object(&content);
            let hash = hash_object(&object);
            for store in stores {
                store.put(&hash, &object)?;
            }
            stats.blobs += 1;
            (Kind::Blob, hash, is_executable(&meta))
        } else {
            bail!("unsupported file type: {}", path.display())
        };
        tree.insert(
            name,
            Entry {
                kind,
                hash,
                executable,
            },
        );
    }
    let object = encode_tree(&tree)?;
    let hash = hash_object(&object);
    for store in stores {
        store.put(&hash, &object)?;
    }
    stats.trees += 1;
    Ok(hash)
}
#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}
pub fn verify_object(store: &dyn ObjectStore, hash: &str) -> Result<Vec<u8>> {
    let bytes = store.get(hash)?;
    ensure!(
        hash_object(&bytes) == hash,
        "object {hash} failed hash verification"
    );
    Ok(bytes)
}
pub fn reachable(store: &dyn ObjectStore, root: &str) -> Result<BTreeSet<String>> {
    fn visit(s: &dyn ObjectStore, h: &str, seen: &mut BTreeSet<String>) -> Result<()> {
        if !seen.insert(h.to_owned()) {
            return Ok(());
        }
        let b = verify_object(s, h)?;
        if b.starts_with(TREE) {
            for e in decode_tree(&b)?.values() {
                visit(s, &e.hash, seen)?
            }
        } else {
            decode_blob(&b)?;
        }
        Ok(())
    }
    let mut seen = BTreeSet::new();
    visit(store, root, &mut seen)?;
    Ok(seen)
}
pub fn flatten(store: &dyn ObjectStore, root: &str) -> Result<BTreeMap<PathBuf, Entry>> {
    fn walk(
        s: &dyn ObjectStore,
        h: &str,
        base: &Path,
        out: &mut BTreeMap<PathBuf, Entry>,
    ) -> Result<()> {
        for (name, e) in decode_tree(&verify_object(s, h)?)? {
            let p = base.join(name);
            if e.kind == Kind::Tree {
                walk(s, &e.hash, &p, out)?
            } else {
                out.insert(p, e);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(store, root, Path::new(""), &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tree_encoding_is_sorted_and_round_trips() {
        let mut t = Tree::new();
        t.insert(
            "z".into(),
            Entry {
                kind: Kind::Blob,
                hash: format!("blake3:{}", "ab".repeat(32)),
                executable: false,
            },
        );
        t.insert(
            "a".into(),
            Entry {
                kind: Kind::Tree,
                hash: format!("blake3:{}", "cd".repeat(32)),
                executable: true,
            },
        );
        let b = encode_tree(&t).unwrap();
        assert_eq!(decode_tree(&b).unwrap(), t);
        assert_eq!(b, encode_tree(&t).unwrap());
    }
}
