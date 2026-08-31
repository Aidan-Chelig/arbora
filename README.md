# Arbora

Arbora synchronizes asset directories through an immutable, BLAKE3-addressed
Merkle tree. Git tracks `assets.lock`; file and tree objects live outside Git in
a local object store and a reusable cache.

## Quick start

```console
arbora init
# add files under assets/
arbora push
arbora status
arbora pull
arbora verify
```

`arbora init` creates `.arbora.toml`, `assets/`, and a local remote at
`.arbora-remote/`. Commit `.arbora.toml` and `assets.lock`, but add `assets/` and
`.arbora-remote/` to `.gitignore`. The default cache is the platform cache
directory under `arbora/`; set `[cache].path` in `.arbora.toml` to override it.

Use `arbora diff` for changes between the locked root and the workspace, or
`arbora diff ROOT_A ROOT_B` for two stored roots. `arbora sync` pulls a clean
workspace and pushes a changed workspace; `arbora sync --pull` explicitly
discards local changes. Pull removes stale files by default. Pass `--keep-stale`
to merge the locked tree into the workspace instead.

## Remote backends

The default local remote uses a path relative to the project:

```toml
[remote]
type = "local"
path = ".arbora-remote"
```

HTTP remotes are anonymous and read-only. The URL is the directory above the
`objects/` layout, and an optional prefix is prepended to every object key:

```toml
[remote]
type = "http"
url = "https://cdn.example.com"
prefix = "my-project"
```

S3 remotes support AWS S3 and compatible services such as R2, B2, MinIO,
Tigris, and Garage:

```toml
[remote]
type = "s3"
bucket = "my-assets"
region = "us-east-1"
prefix = "arbora/my-project"
# endpoint = "https://custom-s3.example.com"
# force_path_style = true
# profile = "publisher"
# anonymous = true
```

By default the AWS SDK credential chain checks environment variables (including
session tokens and `AWS_PROFILE`), shared AWS config/credential files, web
identity, and container or instance roles. `profile` selects a shared profile.
For exceptional cases, `access_key_id`, `secret_access_key`, and
`session_token` can be placed in `[remote]`, but avoid committing secrets;
environment variables or profiles are preferred. Set `anonymous = true` for a
public S3 bucket. Custom endpoints commonly also need `force_path_style = true`.
