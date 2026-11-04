# blitz-dom 0.3.0-beta.1 local patch

This directory contains the published crates.io archive extracted without
changes, followed by the source changes listed below. Published manifests,
assets and sources are retained. Cargo's local `.cargo-ok` extraction marker is
not part of the archive and is not included.

- Registry archive: `https://static.crates.io/crates/blitz-dom/blitz-dom-0.3.0-beta.1.crate`
- Archive SHA-256: `a9f438e914c626f4f56cd58cd88045e164e1ac32e12d7c24d09c25121328161e`
- Published repository: `https://github.com/dioxuslabs/blitz`
- Published VCS commit: `11279518661e1e0af3ee141c232daffa8968d3fe`
- Published VCS path: `packages/blitz-dom`
- License: `MIT OR Apache-2.0`. The registry archive omits the license files;
  `LICENSE-MIT` and `LICENSE-APACHE` were obtained from this exact repository commit.

## Local changes

Only `src/document.rs`, `src/mutator.rs`, `src/resolve.rs`, and
`src/layout/{construct,inline,mod}.rs` differ from the published source:

- Author stylesheets enter the cascade in connected DOM preorder rather than
  numeric slab-node order. Replacing a response preserves its source position.
- `sync_stylesheets` reconciles order and active link state (`rel`, `href`,
  `disabled`, connection). Inactive nodes retain their original parsed objects,
  preserving response URL bases and stylesheet identity across moves/reinsertion.
- `remove_stylesheet_for_node` safely and idempotently forgets a node's sheet.
  Node destruction clears retained sheets before slab IDs can be reused. The
  mutator delegates explicit stylesheet removal to this method.
- Resolve reconciles stylesheet state before style traversal. Detachment
  deactivates a sheet instead of destroying its parsed response.
- Comment nodes do not generate layout boxes or influence inline/block
  classification. This preserves fragment anchors without swallowing siblings.
- Absolute descendants whose containing block skips static layout ancestors
  use the existing absolute layout algorithm with the nearest positioned
  ancestor's padding box, or the initial viewport. Positions remain relative
  to their original layout parent so paint, hit testing and observation agree.
  Direct-container placement remains with Taffy, preserving its grid/flex rules.
- Non-replaced inline element observation boxes come from the same Parley
  glyph-run bounds used for text/background painting, including nested spans.
  Embedded replaced boxes keep their formatting-root layout parent after
  shaping; inline span/style ownership does not add a second geometry offset.
- Iframes participate as replaced inline boxes with independent HTML default
  dimensions (300 by 150) and width/height attributes; CSS sizing still wins.

No dependency versions or feature defaults changed. The workspace patch is
pinned to this exact published version. Regression coverage lives in
`crates/oxibrowser-render/tests/stylesheet_lifecycle.rs`,
`crates/oxibrowser-render/tests/layout_observation.rs`, and the existing render
and preloaded stylesheet suites:

```sh
cargo test -p oxibrowser-render
```
