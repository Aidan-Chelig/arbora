use anyhow::{Context, Result, ensure};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const CONFIG: &str = ".arbora.toml";
pub const LOCK: &str = "assets.lock";
pub const IGNORE: &str = ".aboraignore";

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub remote: Remote,
    pub workspace: Workspace,
    #[serde(default)]
    pub cache: Cache,
    #[serde(default)]
    pub transfer: Transfer,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Remote {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: Option<PathBuf>,
    pub url: Option<String>,
    pub bucket: Option<String>,
    #[serde(default)]
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    #[serde(default)]
    pub anonymous: bool,
    #[serde(default)]
    pub force_path_style: bool,
    #[serde(default = "default_retry_max_attempts")]
    pub retry_max_attempts: u32,
    #[serde(default = "default_retry_max_backoff_ms")]
    pub retry_max_backoff_ms: u64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub path: PathBuf,
    #[serde(default = "default_remove_stale")]
    pub remove_stale: bool,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    pub path: Option<PathBuf>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Transfer {
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}
impl Default for Transfer {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    pub root: String,
}
fn default_remove_stale() -> bool {
    true
}
fn default_retry_max_attempts() -> u32 {
    4
}
fn default_retry_max_backoff_ms() -> u64 {
    5_000
}
fn default_concurrency() -> usize {
    8
}

impl Config {
    pub fn load(project: &Path) -> Result<Self> {
        let path = project.join(CONFIG);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read {}; run `arbora init` first", path.display()))?;
        Ok(toml::from_str(&text)?)
    }
    pub fn workspace(&self, project: &Path) -> PathBuf {
        project.join(&self.workspace.path)
    }
    pub fn cache(&self) -> Result<PathBuf> {
        if let Some(p) = &self.cache.path {
            return Ok(p.clone());
        }
        Ok(dirs::cache_dir()
            .context("cannot determine cache directory")?
            .join("arbora"))
    }
    pub fn concurrency(&self) -> Result<usize> {
        ensure!(
            (1..=64).contains(&self.transfer.concurrency),
            "transfer concurrency must be between 1 and 64"
        );
        Ok(self.transfer.concurrency)
    }
    pub fn ignore(&self, project: &Path) -> Result<Gitignore> {
        let path = project.join(IGNORE);
        let mut builder = GitignoreBuilder::new(self.workspace(project));
        builder.add_line(None, &format!("/{IGNORE}"))?;
        if path.exists()
            && let Some(error) = builder.add(&path)
        {
            return Err(error).with_context(|| format!("parse {}", path.display()));
        }
        builder.build().map_err(Into::into)
    }
}
pub fn read_lock(project: &Path) -> Result<Lock> {
    let path = project.join(LOCK);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("read {}; run `arbora push` first", path.display()))?;
    let lock: Lock = toml::from_str(&text)?;
    ensure!(
        lock.version == 1,
        "unsupported lock version {}",
        lock.version
    );
    Ok(lock)
}
pub fn write_lock(project: &Path, root: &str) -> Result<()> {
    let text = toml::to_string_pretty(&Lock {
        version: 1,
        root: root.into(),
    })?;
    fs::write(project.join(LOCK), text)?;
    Ok(())
}
