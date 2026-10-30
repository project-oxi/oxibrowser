# wreq-util 3.0.0-rc.13 local patch

This directory contains the published `wreq-util` 3.0.0-rc.13 crate, extracted
without changes from its verified crates.io archive, followed by the local
changes listed below. The normalized published `Cargo.toml`, original manifest,
source files, tests, examples, and upstream documentation are retained
(55/55 archive files).

- Registry archive: `https://static.crates.io/crates/wreq-util/wreq-util-3.0.0-rc.13.crate`
- Archive SHA-256: `9c1565487def020541fed7fe1d2564770e78b9abab56fea1412623f1071eadfd`
  (matches the `checksum` this package carried in `Cargo.lock` before patching)
- Upstream repository: `https://github.com/0x676e67/wreq-util`
- Published VCS commit: `c42fbe0d1ca02fe67499b8b58017bdc7acf771d5`
- Published VCS path: `` (repository root)
- License: `Apache-2.0`. The `LICENSE` file is retained from the archive
  itself; no license files had to be fetched from upstream.

## Local changes

Exactly two manifest lines; no source, feature, profile, or license changes:

1. `Cargo.toml` (normalized published manifest), `[dependencies.zstd]`:
   `version = "0.13.3"` → `version = "0.14.3"`
2. `Cargo.toml.orig`, same dependency: `zstd = { version = "0.13.3", optional = true }`
   → `zstd = { version = "0.14.3", optional = true }`

Rationale: published wreq-util rc.13/rc14 require `zstd ^0.13.3`, which
resolves zstd 0.13.3 / zstd-safe 7.2.4 / zstd-sys 2.0.16 — covered by
RUSTSEC-2026-0391, -0398, -0403, and -0410. The fixed chain is zstd 0.14.3 /
zstd-safe 8.2.0 / zstd-sys 2.1.2. The crate's own zstd usage
(`src/emulate/compress.rs`, `zstd::stream::{Decoder, Encoder}`) is
API-compatible across 0.13 → 0.14, so no source change is needed. The older
rc.11 release without this dependency does not provide the Chrome 149
emulation used by this workspace.

Selection: the root `Cargo.toml` pins `wreq-util = "=3.0.0-rc.13"` and selects
this exact version through `[patch.crates-io]`
(`wreq-util = { path = "vendor/wreq-util", version = "=3.0.0-rc.13" }`).
`compression-codecs` is moved to 0.4.40 (its zstd 0.14 line) via targeted
`cargo update -p compression-codecs --precise 0.4.40`. Only this package and
the zstd/zstd-safe/zstd-sys chain changed versions in the root lockfile.
The wreq version and all enabled features remain unchanged.
The retained upstream Cargo.lock is an archive artifact; workspace builds
resolve dependencies using the root Cargo.lock.

Revisit this override when a compatible published wreq-util release adopts
the fixed zstd chain; preserve the profile and rerun the checks below.

Verify the single fixed chain and the advisory pass with:

```sh
cargo tree -i zstd        # zstd v0.14.3 only
cargo tree -i zstd-safe   # zstd-safe v8.2.0 only
cargo deny check advisories
cargo test -p oxibrowser-core --test binary_fetch  # includes zstd response decoding
```

The workspace's `[patch.crates-io]` selects only this exact version; other
wreq crates continue to use their registry packages.
