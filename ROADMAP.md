# Arbora implementation roadmap

This checklist tracks the remaining essentials identified after the initial
local, HTTP, S3, cache, lock-file, and ignore-file implementation.

- [x] Make global-cache garbage collection safe across multiple projects by
      maintaining project root references and conservatively protecting legacy
      cache objects.
- [x] Stream hashing, object reads/writes, network transfers, and
      materialization so memory use does not scale with the largest asset.
- [x] Add bounded parallel downloads and uploads without overwhelming remote
      storage providers.
- [x] Materialize cached blobs using reflinks, then hardlinks, then copies as a
      portable fallback.
- [x] Exercise the S3 backend against an actual S3-compatible service in CI,
      with an optional R2 smoke-test path.
- [x] Add prefix-scoped remote garbage collection with dry-run reports,
      explicit confirmation, Git-history and manual root retention, grace
      periods, and batched S3 deletion.

Each item is checked only after its implementation, regression tests, strict
lint, and relevant integration tests pass.
