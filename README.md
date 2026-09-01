# Arbora

Arbora keeps large asset trees out of Git without giving up reproducibility.
Files and directories are stored as immutable, BLAKE3-addressed objects; Git
only needs to track a small `assets.lock` file containing the expected root
hash.

It is roughly **rsync for content-addressed object storage**.

## Features

- Deterministic Merkle trees with one hash identifying the complete asset state
- Local filesystem, anonymous read-only HTTP/HTTPS, and S3-compatible remotes
- AWS Signature V4 and the standard AWS credential provider chain
- Verified downloads: corrupt objects are rejected before materialization
- A global cache shared by projects and worktrees
- Streaming hashing and transfers with memory use independent of blob size
- File-level status and diffs without a separate revision-history system
- Executable-bit preservation and atomic workspace replacement on pull
- Reflink, hardlink, then streamed-copy materialization fallbacks

Arbora currently stores each file as one blob. Content-defined chunking and
remote garbage collection are intentionally out of scope for the initial
version.

## Installation

Arbora requires Rust 1.98 or newer.

```console
cargo install --git https://github.com/Aidan-Chelig/arbora
```

To build from a checkout:

```console
git clone https://github.com/Aidan-Chelig/arbora.git
cd arbora
nix develop             # optional; provides the pinned Rust toolchain
cargo build --release
```

The resulting executable is `target/release/arbora`.

## Quick start

```console
cd my-project
arbora init

# Add files beneath assets/, then publish them.
arbora push
git add .arbora.toml .aboraignore assets.lock

# On another checkout:
arbora pull
arbora verify
```

`arbora init` creates:

```text
.arbora.toml       Project configuration
.aboraignore        Git-style asset exclusion rules
assets/            Asset workspace
.arbora-remote/    Default local object store
```

Commit `.arbora.toml`, `.aboraignore`, and `assets.lock`. Usually the following
belongs in the project's `.gitignore`:

```gitignore
/assets/
/.arbora-remote/
```

The default cache is the platform cache directory under `arbora/` (typically
`~/.cache/arbora` on Linux). It can be overridden in `.arbora.toml`:

```toml
[cache]
path = "/fast-disk/arbora-cache"

[transfer]
# Shared upper bound for simultaneous uploads or downloads (1-64).
concurrency = 8
```

The shared cache records one active root per project. `arbora gc` retains the
union of every registered project root rather than treating the current project
as the cache's sole owner. Objects created before this reference registry was
introduced are conservatively protected during migration.

Transfers are bounded and parallel: `concurrency` limits both simultaneous
uploads during `push` and simultaneous downloads during `pull`, `diff`,
`verify`, and `gc`. Objects are streamed through the cache, so memory use does
not grow with the size of the largest asset.

## Commands

| Command | Purpose |
| --- | --- |
| `arbora init` | Create the initial configuration and workspace |
| `arbora status` | Compare the workspace with `assets.lock` |
| `arbora push` | Upload missing objects and update `assets.lock` |
| `arbora pull` | Materialize the tree named by `assets.lock` |
| `arbora sync` | Pull a clean workspace or push a changed workspace |
| `arbora diff` | Show added, modified, and deleted files |
| `arbora verify` | Verify the remote object graph and workspace root |
| `arbora gc` | Remove cache objects not referenced by any registered project |

`arbora diff ROOT_A ROOT_B` compares two stored roots. With no roots it compares
the locked root to the workspace.

Pull replaces the workspace and removes stale files by default. Use
`arbora pull --keep-stale` to merge locked files into the existing workspace.
`arbora sync --pull` explicitly discards local asset changes and restores the
locked tree.

Materialization automatically tries a copy-on-write reflink, then a hardlink,
and finally a streamed copy. Mode-specific cache views preserve executable bits
without requiring filesystem-specific setup.

Use `--project PATH` with any command to operate on a project other than the
current directory.

## Ignoring files

Place Git-style patterns in `.aboraignore` at the project root. Patterns are
matched relative to the configured workspace and affect every operation that
scans local assets, including `status`, `push`, `sync`, `diff`, and `verify`.

```gitignore
# Temporary exports anywhere in the tree
*.tmp

# A generated directory
cache/

# Negation is supported
!keep.tmp
```

Comments, anchored paths, directory rules, `**`, and `!` negation follow
`.gitignore` semantics. Ignored files are neither uploaded nor represented in
the root hash. A normal pull preserves ignored local files while removing other
stale paths.

## Configuration

### Local filesystem

Local paths are resolved relative to the project:

```toml
[remote]
type = "local"
path = ".arbora-remote"

[workspace]
path = "assets"
remove_stale = true
```

### HTTP/HTTPS

HTTP remotes are anonymous and read-only. The URL points to the directory above
the object layout. An optional prefix is prepended to every object key.

```toml
[remote]
type = "http"
url = "https://cdn.example.com"
prefix = "my-project"
```

Objects are requested using `HEAD` and `GET`, for example:

```text
https://cdn.example.com/my-project/objects/ab/cdef...
```

An HTTP remote can be used by `pull`, `diff`, and `verify`. Once the locked tree
is cached, `status` works without contacting the remote. `push` requires a
writable local or S3 remote.

### S3-compatible storage

The S3 backend works with AWS S3 and compatible providers such as Cloudflare
R2, Backblaze B2, MinIO, Tigris, and Garage.

```toml
[remote]
type = "s3"
bucket = "my-assets"
region = "us-east-1"
prefix = "arbora/my-project"

# Common for non-AWS providers:
# endpoint = "https://custom-s3.example.com"
# force_path_style = true

# Optional authentication controls:
# profile = "publisher"
# anonymous = true

# Bounded retries for transient failures such as R2 HTTP 503 responses:
# retry_max_attempts = 4
# retry_max_backoff_ms = 5000
```

By default, Arbora uses the standard AWS credential chain, including:

1. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and `AWS_SESSION_TOKEN`
2. Shared AWS configuration and credentials files
3. `AWS_PROFILE` or the configured `profile`
4. Web identity, container credentials, and instance roles

Explicit `access_key_id`, `secret_access_key`, and `session_token` fields are
also supported under `[remote]`, but environment variables or shared profiles
are strongly preferred so credentials are not committed to Git.

Set `anonymous = true` for unsigned access to a public bucket. Anonymous mode
cannot be combined with explicit credentials.

S3 requests use jittered exponential backoff for transient errors, including
Cloudflare R2 HTTP 503 `ServiceUnavailable` responses. The default is four
total attempts (the initial request and up to three retries), with a five-second
maximum delay between attempts. `retry_max_attempts` accepts values from 1
(retries disabled) through 10, so retries always remain bounded.

Recommended permissions are:

- Readers: `GET`, `HEAD`
- Publishers: `GET`, `HEAD`, `PUT`

Arbora does not need delete permission for normal operation.

## Storage layout

Objects use the same layout on every backend:

```text
objects/<first two hash characters>/<remaining hash characters>
```

Blob and tree objects include a type marker in the hashed bytes. Tree entries
are serialized deterministically in name order and contain the child name,
object type, hash, and executable flag. Changing one file changes that blob,
its parent tree, and each ancestor up to the root; unchanged subtrees retain
their hashes.

Downloaded objects are always hashed again before Arbora trusts them.

## Operational notes

- Uploads are immutable and idempotent; objects already present remotely are
  skipped.
- `assets.lock` is updated only after every required object is uploaded.
- Pulls stage a complete replacement before updating the workspace, avoiding a
  partially materialized asset tree.
- Cache garbage collection is local only. Arbora never deletes remote objects.
- Each file is currently one object, so changing part of a large file uploads
  that entire file again.

## Development

```console
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
nix flake check path:. --no-build
```

The loopback HTTP integration test is ignored in restricted sandboxes. Run it
explicitly where local sockets are available:

```console
cargo test --test http_store -- --ignored
```

CI also runs `tests/s3_compatible.rs` against a real MinIO server. The same test
can smoke-test Cloudflare R2 from a manually dispatched GitHub Actions run when
the `R2_ENDPOINT`, `R2_BUCKET`, `R2_ACCESS_KEY_ID`, and
`R2_SECRET_ACCESS_KEY` repository secrets are configured. The test exercises
missing-object `HEAD`, streaming `PUT`, existing-object `HEAD`, and streaming
`GET` using the production S3 adapter.
