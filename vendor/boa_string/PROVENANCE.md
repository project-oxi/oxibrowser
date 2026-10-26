# boa_string 0.22.0 local patch

This directory contains the published `boa_string` 0.22.0 crate, extracted
without changes from its verified crates.io archive, followed by the local
changes listed below. The normalized published `Cargo.toml`, original manifest,
source files, and upstream documentation are retained.

- Registry archive: `https://static.crates.io/crates/boa_string/boa_string-0.22.0.crate`
- Archive SHA-256: `30bf86c2bcff5bf2547acbfc516c5d3108ca527cb131511d749bf6fbef762c0e`
- Upstream repository: `https://github.com/boa-dev/boa`
- Published VCS commit: `337a3668a0dc86dd401ea20906e782249a64a228`
- Published VCS path: `core/string`
- License: `Unlicense OR MIT`. Both license files were added from that exact
  upstream repository commit; the registry archive does not include them.

Upstream crates.io metadata checked on 2026-10-26 lists 0.22.0 as the latest
release. Both the v0.22 tag and current main branch still lack `Windows::nth`.

## Local changes

`src/iter.rs` delegates `Windows::nth` directly to the underlying Latin1 or
UTF-16 slice-window iterator. This preserves constant-time skipping and normal
iterator consumption. It avoids revisiting every earlier window when
`JsStr::index_of` calls `windows(...).skip(from_index)`, which is repeated by
JavaScript `String.prototype.split`. Search/string semantics are unchanged.
The same file adds unit cases for consumption, exhaustion, Latin1 bytes, UTF-16
surrogates, mixed-encoding searches, and empty/out-of-range inputs.

Run the full crate tests with `cargo test -p boa_string`. The manual benchmark
`benchmarks/string_split.py` runs actual JavaScript `String.split` in an
optimized OxiBrowser binary against a supplied UTF-8 source file and a forced
UTF-16 variant. Compare identical inputs against before/after binaries:

```sh
python3 vendor/boa_string/benchmarks/string_split.py /path/to/oxibrowser /path/to/source.frag --repetitions 10
```

The benchmark checks output counts, reports timings, and contains no timing
threshold. The workspace's `[patch.crates-io]` selects only this exact version;
other Boa crates continue to use their registry packages.
