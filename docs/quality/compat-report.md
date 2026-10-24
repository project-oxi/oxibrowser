# Compatibility corpus report

Generated from `crates/oxibrowser/tests/data/compat_corpus.txt` (epoch 1792844970).

| URL | status | bytes | JS errors | note / detail |
|---|---|---|---|---|
| https://example.com | PASS | 156 | 1 | static control |
| https://example.org | PASS | 156 | 1 | static control |
| https://news.ycombinator.com | PASS | 4375 | 0 | classic SSR |
| https://en.wikipedia.org/wiki/Main_Page | PASS | 16011 | 0 | large document |
| https://en.wikipedia.org/wiki/JavaScript | PASS | 76346 | 0 | very large document |
| https://developer.chrome.com/docs/devtools | PASS | 9812 | 0 | modern SSG |
| https://developer.mozilla.org/en-US/docs/Web/API | PASS | 28313 | 5 | MDN SSR + hydration |
| https://www.rust-lang.org | PASS | 3903 | 0 | marketing SSG |
| https://doc.rust-lang.org/book/ | PASS | 1313 | 4 | docs site |
| https://www.python.org | PASS | 7283 | 6 | static |
| https://www.w3.org | PASS | 5807 | 1 | standards org |
| https://stackoverflow.com/questions/231767 | FETCH_ERR | 0 | 0 | Q&A SSR |
| https://github.com/microsoft/vscode | PASS | 9183 | 9 | heavy app page |
| https://news.ycombinator.com/newest | PASS | 4489 | 0 | SSR list |
| https://naver.com | PASS | 294 | 4 | KR portal |
| https://www.daum.net | EMPTY | 0 | 7 | KR portal |
| https://www.bbc.com/news | PASS | 9824 | 13 | heavy media |
| https://httpbin.org/forms/post | PASS | 181 | 0 | forms |
| https://open.spotify.com | EMPTY | 0 | 1 | heavy SPA variant |
| https://www.spotify.com | EMPTY | 0 | 1 | heavy SPA — SIGABRT (iter.1 fix) → EMPTY; 0.22 parses web-player, blocker now `not a constructor` (iter.2) |
| https://www.figma.com | PASS | 7103 | 1 | heavy WASM/worker app — stack-overflow abort 2026-10-24 AM → PASS after isolation fix |

**Pass rate: 17/21 (80%) · crashes: 0 · timeouts: 0**
