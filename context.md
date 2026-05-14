# CDP Code Patterns Summary

## 1. DispatchContext Struct (domains/mod.rs)

```rust
pub struct DispatchContext {
    /// Browser session (read/write for navigation, DOM access, JS eval).
    pub session: Arc<RwLock<Session>>,
    /// Event sender for emitting CDP events to the client.
    pub events: EventSender,
}

pub type DomainResult = std::result::Result<Option<Value>, CdpError>;
```

**Pattern**: Async domain handlers receive `&DispatchContext`, sync handlers don't need async.

---

## 2. fetch.rs Key Patterns

### handle() dispatch
```rust
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "enable" => enable(params, ctx),
        "disable" => disable(ctx),
        // ...
    }
}
```

### enable/disable — state stored on EventSender
```rust
fn enable(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_fetch_enabled(true);
    ctx.events.set_fetch_patterns(patterns.clone());
    Ok(Some(json!({})))
}

fn disable(ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_fetch_enabled(false);
    ctx.events.set_fetch_patterns(vec![]);
    Ok(Some(json!({})))
}
```

### emit_request_paused — takes EventSender directly
```rust
pub fn emit_request_paused(
    events: &EventSender,
    request_id: &str,
    url: &str,
    method: &str,
    headers: &[(String, String)],
    resource_type: &str,
) {
    events.send_fetch_event("Fetch.requestPaused", json!({...}));
}
```

**Key insight**: Event emission functions take `&EventSender` (not `&DispatchContext`), so they can be called from anywhere (network layer).

---

## 3. CdpSession Creates DispatchContext (session.rs)

```rust
async fn handle_text_message(&mut self, text: &str) -> anyhow::Result<()> {
    // ... parse request ...

    // Create dispatch context with session + event sender
    let ctx = DispatchContext {
        session: self.session.clone(),
        events: self.event_sender.clone(),
    };

    // Dispatch to domain handler
    let response = match domains::dispatch(&request.method, request.params, &ctx).await {
        // ...
    };
}
```

**Session fields**:
```rust
pub struct CdpSession {
    browser: Arc<Browser>,
    session: Arc<RwLock<oxibrowser_core::session::Session>>,
    event_sender: EventSender,
    // ...
}
```

---

## Pattern Summary for Task 1

| Concern | Pattern |
|---------|---------|
| Domain handler signature | `async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult` |
| State management | Store flags on `EventSender` via `set_*()` methods |
| Event emission from network layer | `emit_*()` functions take `&EventSender`, called in `HttpClient` |
| Error codes | `CdpError { code: -32601, message: "..." }` for unknown methods, `-32602` for bad params |
| Response format | `Ok(Some(json!({})))` for success, `Err(CdpError)` for failures |
