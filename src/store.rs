use anyhow::{Context, Result, bail, ensure};
use aws_config::retry::RetryConfig;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
use reqwest::{StatusCode, blocking::Client as HttpClient};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub trait ObjectStore: Sync {
    fn get(&self, hash: &str) -> Result<Vec<u8>>;
    fn put(&self, hash: &str, bytes: &[u8]) -> Result<bool>;
    fn exists(&self, hash: &str) -> Result<bool>;
    fn download_to(&self, hash: &str, destination: &Path) -> Result<()> {
        fs::write(destination, self.get(hash)?)?;
        Ok(())
    }
    fn upload_from(&self, hash: &str, source: &Path) -> Result<bool> {
        self.put(hash, &fs::read(source)?)
    }
    fn put_blob_file(&self, hash: &str, source: &Path) -> Result<bool> {
        let mut object = crate::merkle::blob_prefix().to_vec();
        fs::File::open(source)?.read_to_end(&mut object)?;
        self.put(hash, &object)
    }
}

pub fn object_key(hash: &str, prefix: &str) -> Result<String> {
    let hex = hash.strip_prefix("blake3:").unwrap_or(hash);
    ensure!(
        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid object hash: {hash}"
    );
    let key = format!("objects/{}/{}", &hex[..2], &hex[2..]);
    Ok(if prefix.trim_matches('/').is_empty() {
        key
    } else {
        format!("{}/{key}", prefix.trim_matches('/'))
    })
}

#[derive(Clone, Debug)]
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn object_path(&self, hash: &str) -> Result<PathBuf> {
        Ok(self.root.join(object_key(hash, "")?))
    }
    pub fn install_file(&self, hash: &str, source: &Path) -> Result<bool> {
        let path = self.object_path(hash)?;
        if path.exists() {
            return Ok(false);
        }
        fs::create_dir_all(path.parent().unwrap())?;
        match fs::rename(source, &path) {
            Ok(()) => Ok(true),
            Err(_) if path.exists() => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HttpStore {
    base_url: String,
    prefix: String,
    client: HttpClient,
}

impl HttpStore {
    pub fn new(base_url: impl Into<String>, prefix: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        ensure!(
            base_url.starts_with("http://") || base_url.starts_with("https://"),
            "HTTP remote URL must use http:// or https://"
        );
        Ok(Self {
            base_url,
            prefix: prefix.into(),
            client: HttpClient::builder().build()?,
        })
    }
    fn url(&self, hash: &str) -> Result<String> {
        Ok(format!(
            "{}/{}",
            self.base_url,
            object_key(hash, &self.prefix)?
        ))
    }
}

impl ObjectStore for HttpStore {
    fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let url = self.url(hash)?;
        let response = self.client.get(&url).send()?.error_for_status()?;
        Ok(response.bytes()?.to_vec())
    }
    fn put(&self, _hash: &str, _bytes: &[u8]) -> Result<bool> {
        bail!("HTTP remotes are read-only; configure a local or S3 remote to push")
    }
    fn exists(&self, hash: &str) -> Result<bool> {
        let response = self.client.head(self.url(hash)?).send()?;
        match response.status() {
            status if status.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            status => bail!("HTTP HEAD failed with {status}"),
        }
    }
    fn download_to(&self, hash: &str, destination: &Path) -> Result<()> {
        let mut response = self
            .client
            .get(self.url(hash)?)
            .send()?
            .error_for_status()?;
        let mut file = fs::File::create(destination)?;
        response.copy_to(&mut file)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct S3Options {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub anonymous: bool,
    pub force_path_style: bool,
    pub retry_max_attempts: u32,
    pub retry_max_backoff_ms: u64,
}

impl Default for S3Options {
    fn default() -> Self {
        Self {
            bucket: String::new(),
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
        }
    }
}

pub struct S3Store {
    client: S3Client,
    runtime: tokio::runtime::Runtime,
    bucket: String,
    prefix: String,
}

impl S3Store {
    pub fn new(options: S3Options) -> Result<Self> {
        ensure!(!options.bucket.is_empty(), "S3 remote requires a bucket");
        ensure!(
            options.access_key_id.is_some() == options.secret_access_key.is_some(),
            "S3 explicit credentials require both access_key_id and secret_access_key"
        );
        ensure!(
            !options.anonymous || options.access_key_id.is_none(),
            "S3 anonymous mode cannot be combined with explicit credentials"
        );
        ensure!(
            (1..=10).contains(&options.retry_max_attempts),
            "S3 retry_max_attempts must be between 1 and 10"
        );
        ensure!(
            options.retry_max_backoff_ms > 0,
            "S3 retry_max_backoff_ms must be greater than zero"
        );
        let runtime = tokio::runtime::Runtime::new()?;
        let shared = runtime.block_on(async {
            let retry_config = RetryConfig::standard()
                .with_max_attempts(options.retry_max_attempts)
                .with_initial_backoff(std::time::Duration::from_millis(100))
                .with_max_backoff(std::time::Duration::from_millis(
                    options.retry_max_backoff_ms,
                ));
            let mut loader = aws_config::from_env().retry_config(retry_config);
            if let Some(region) = options.region.clone() {
                loader = loader.region(aws_config::Region::new(region));
            }
            if let Some(profile) = options.profile.as_deref() {
                loader = loader.profile_name(profile);
            }
            if options.anonymous {
                loader = loader.no_credentials();
            } else if let (Some(access), Some(secret)) = (
                options.access_key_id.clone(),
                options.secret_access_key.clone(),
            ) {
                loader = loader.credentials_provider(Credentials::new(
                    access,
                    secret,
                    options.session_token.clone(),
                    None,
                    "arbora-config",
                ));
            }
            loader.load().await
        });
        let mut builder =
            aws_sdk_s3::config::Builder::from(&shared).force_path_style(options.force_path_style);
        if let Some(endpoint) = options.endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        if options.anonymous {
            builder = builder.allow_no_auth();
        }
        Ok(Self {
            client: S3Client::from_conf(builder.build()),
            runtime,
            bucket: options.bucket,
            prefix: options.prefix,
        })
    }
    fn key(&self, hash: &str) -> Result<String> {
        object_key(hash, &self.prefix)
    }
}

impl ObjectStore for S3Store {
    fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let key = self.key(hash)?;
        self.runtime.block_on(async {
            let output = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .with_context(|| format!("get s3://{}/{key}", self.bucket))?;
            Ok(output.body.collect().await?.into_bytes().to_vec())
        })
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> Result<bool> {
        if self.exists(hash)? {
            return Ok(false);
        }
        let key = self.key(hash)?;
        self.runtime.block_on(async {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(ByteStream::from(bytes.to_vec()))
                .send()
                .await
                .with_context(|| format!("put s3://{}/{key}", self.bucket))?;
            Ok(true)
        })
    }
    fn exists(&self, hash: &str) -> Result<bool> {
        let key = self.key(hash)?;
        self.runtime.block_on(async {
            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(_) => Ok(true),
                Err(error)
                    if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 404) =>
                {
                    Ok(false)
                }
                Err(error) => {
                    Err(error).with_context(|| format!("head s3://{}/{key}", self.bucket))
                }
            }
        })
    }
    fn download_to(&self, hash: &str, destination: &Path) -> Result<()> {
        let key = self.key(hash)?;
        self.runtime.block_on(async {
            let output = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .with_context(|| format!("get s3://{}/{key}", self.bucket))?;
            let mut source = output.body.into_async_read();
            let mut destination = tokio::fs::File::create(destination).await?;
            tokio::io::copy(&mut source, &mut destination).await?;
            Ok(())
        })
    }
    fn upload_from(&self, hash: &str, source: &Path) -> Result<bool> {
        if self.exists(hash)? {
            return Ok(false);
        }
        let key = self.key(hash)?;
        self.runtime.block_on(async {
            let body = ByteStream::from_path(source).await?;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(body)
                .send()
                .await
                .with_context(|| format!("put s3://{}/{key}", self.bucket))?;
            Ok(true)
        })
    }
}

impl ObjectStore for LocalStore {
    fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.object_path(hash)?;
        fs::read(&path).with_context(|| format!("read object {}", path.display()))
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> Result<bool> {
        let path = self.object_path(hash)?;
        if path.exists() {
            return Ok(false);
        }
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            &hash[hash.len() - 8..]
        ));
        fs::write(&tmp, bytes)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(true),
            Err(_) if path.exists() => {
                let _ = fs::remove_file(tmp);
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }
    fn exists(&self, hash: &str) -> Result<bool> {
        Ok(self.object_path(hash)?.is_file())
    }
    fn download_to(&self, hash: &str, destination: &Path) -> Result<()> {
        fs::copy(self.object_path(hash)?, destination)?;
        Ok(())
    }
    fn upload_from(&self, hash: &str, source: &Path) -> Result<bool> {
        let destination = self.object_path(hash)?;
        if destination.exists() {
            return Ok(false);
        }
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::copy(source, destination)?;
        Ok(true)
    }
    fn put_blob_file(&self, hash: &str, source: &Path) -> Result<bool> {
        let path = self.object_path(hash)?;
        if path.exists() {
            return Ok(false);
        }
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".blob-{}-{}", std::process::id(), &hash[7..15]));
        let mut output = fs::File::create(&temporary)?;
        output.write_all(crate::merkle::blob_prefix())?;
        std::io::copy(&mut fs::File::open(source)?, &mut output)?;
        drop(output);
        match fs::rename(&temporary, &path) {
            Ok(()) => Ok(true),
            Err(_) if path.exists() => {
                let _ = fs::remove_file(temporary);
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_are_validated_and_prefixes_normalized() {
        let hash = format!("blake3:{}", "ab".repeat(32));
        assert_eq!(
            object_key(&hash, "/project/assets/").unwrap(),
            format!("project/assets/objects/ab/{}", "ab".repeat(31))
        );
        assert!(object_key("blake3:not-a-hash", "").is_err());
    }
}
