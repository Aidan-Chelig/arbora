use anyhow::{Context, Result};
use arbora::{
    merkle::{blob_object, hash_object},
    store::{ObjectStore, S3Options, S3Store},
};
use std::{env, fs};

#[test]
#[ignore = "requires an S3-compatible service configured through ARBORA_S3_TEST_* variables"]
fn put_head_get_round_trip() -> Result<()> {
    let endpoint = env::var("ARBORA_S3_TEST_ENDPOINT").context("ARBORA_S3_TEST_ENDPOINT")?;
    let bucket = env::var("ARBORA_S3_TEST_BUCKET").context("ARBORA_S3_TEST_BUCKET")?;
    let access_key = env::var("ARBORA_S3_TEST_ACCESS_KEY").context("ARBORA_S3_TEST_ACCESS_KEY")?;
    let secret_key = env::var("ARBORA_S3_TEST_SECRET_KEY").context("ARBORA_S3_TEST_SECRET_KEY")?;
    let region = env::var("ARBORA_S3_TEST_REGION").unwrap_or_else(|_| "auto".into());
    let prefix = env::var("ARBORA_S3_TEST_PREFIX")
        .unwrap_or_else(|_| format!("arbora-integration/{}", std::process::id()));
    let store = S3Store::new(S3Options {
        bucket,
        prefix,
        endpoint: Some(endpoint),
        region: Some(region),
        access_key_id: Some(access_key),
        secret_access_key: Some(secret_key),
        force_path_style: env::var("ARBORA_S3_TEST_PATH_STYLE").is_ok_and(|value| value == "true"),
        ..S3Options::default()
    })?;
    let object = blob_object(b"arbora S3-compatible integration test\n");
    let hash = hash_object(&object);
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source.object");
    let destination = temp.path().join("downloaded.object");
    fs::write(&source, &object)?;

    assert!(store.upload_from(&hash, &source)?);
    assert!(store.exists(&hash)?);
    assert!(!store.upload_from(&hash, &source)?);
    store.download_to(&hash, &destination)?;
    assert_eq!(fs::read(destination)?, object);
    let listed = store.list_objects()?;
    assert!(listed.iter().any(|item| item.hash == hash && item.size > 0));
    store.delete_objects(std::slice::from_ref(&hash))?;
    assert!(!store.exists(&hash)?);
    Ok(())
}
