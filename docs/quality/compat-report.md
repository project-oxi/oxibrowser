# Compatibility corpus report

Generated from `crates/oxibrowser/tests/data/compat_corpus.txt` (epoch 1792832475).

| URL | status | bytes | JS errors | note / detail |
|---|---|---|---|---|
| https://example.com | PASS | 156 | 1 | static control |
| https://news.ycombinator.com | PASS | 4332 | 0 | classic SSR |
| https://en.wikipedia.org/wiki/Main_Page | PASS | 16159 | 0 | large document |
| https://developer.chrome.com/docs/devtools | PASS | 9812 | 1 | modern SSG |
| https://www.spotify.com | EMPTY | 0 | 4 | heavy SPA — SIGABRT 2026-10-24 |
| https://www.figma.com | PASS | 7103 | 1 | heavy WASM/worker app — stack-overflow abort 2026-10-24 |

**Pass rate: 5/6 (83%) · crashes: 0 · timeouts: 0**
