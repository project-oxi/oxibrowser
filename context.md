# CDP Page Domain Handler Context Analysis

## Dispatch Signature Pattern

The `page::handle()` function is declared as `async fn`:

```rust
// Line 18-19 in page.rs
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
```

All individual handler functions receive `ctx: &DispatchContext` as their third parameter:

| Handler | Signature | Uses ctx.session |
|---------|-----------|------------------|
| `enable` | `fn(ctx)` | No (only ctx.events) |
| `disable` | `fn(ctx)` | No (only ctx.events) |
| `setLifecycleEventsEnabled` | `fn(params, ctx)` | No |
| `getFrameMetrics` | `fn()` | No |
| `navigate` | `async fn(params, ctx)` | **Yes** — write lock |
| `reload` | `async fn(params, ctx)` | **Yes** — write lock |
| `getFrameTree` | `async fn(ctx)` | **Yes** — read lock |
| `captureScreenshot` | `async fn(params, ctx)` | **Yes** — read lock |
| `printToPDF` | `fn(params)` | **No** — placeholder only |

### Mixed Sync/Async in Async Context

The dispatch router calls `page::handle(...).await`, which awaits the entire function. This means:

- Sync handlers (`fn`) are valid — they return `impl Future<Output = DomainResult>` which `.await` waits on
- Async handlers (`async fn`) work naturally with `.await`
- No breaking change to router required

## captureScreenshot Has ctx Access

`capture_screenshot` (lines 196-253) already demonstrates the pattern:

```rust
async fn capture_screenshot(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    // ...
    #[cfg(feature = "render")]
    {
        let guard = ctx.session.read().await;
        let html = guard
            .page()
            .map(|p| p.content())
            .unwrap_or("<html><body></body></html>");
        // ...
    }
}
```

## printToPDF Currently Lacks ctx

`print_to_pdf` (lines 257-271) returns a placeholder:

```rust
fn print_to_pdf(params: Option<Value>) -> DomainResult {
    #[cfg(feature = "render")]
    {
        // TODO: Integrate with session to get actual HTML content
        // For now, return placeholder since we need ctx for session access
        let _ = params;
    }

    Ok(Some(json!({
        "data": "",
        "stream": ""
    })))
}
```

## Answer: Changing printToPDF to async with ctx is Safe

The dispatch router in `mod.rs` (line 58) calls:

```rust
"Page" => page::handle(method_name, params, ctx).await,
```

Since `page::handle` is already `async fn`, changing `print_to_pdf` from:
```rust
"printToPDF" => print_to_pdf(params),
```
to:
```rust
"printToPDF" => print_to_pdf(params, ctx).await,
```

**Will NOT break the dispatch router.** The `.await` on the whole `handle()` call handles both sync and async handlers transparently.

## Required Changes for printToPDF

1. **Signature**: Change from `fn(params)` to `async fn(params, ctx)`
2. **Router call**: Add `.await` to the match arm
3. **Implementation**: Get HTML from `ctx.session.read().await` and call `oxibrowser_render::render_to_pdf()`
4. **Import**: Ensure `oxibrowser_render` is in scope (it's already used by `capture_screenshot`)

## Relevant Files

- `/Volumes/MERCURY/PROJECTS/session-b-cdp-perf/crates/oxibrowser-cdp/src/domains/page.rs` — handler implementations
- `/Volumes/MERCURY/PROJECTS/session-b-cdp-perf/crates/oxibrowser-cdp/src/domains/mod.rs` — dispatch router
- `/Volumes/MERCURY/PROJECTS/session-b-cdp-perf/crates/oxibrowser-render/src/lib.rs` — `render_to_pdf()` entry point
- `/Volumes/MERCURY/PROJECTS/session-b-cdp-perf/crates/oxibrowser-render/src/config.rs` — `RenderConfig` builder API