//! The agent world — a dedicated boa `Context` on its own thread
//! (execution design M1).
//!
//! The world never touches the session or any shared lock: native
//! functions marshal [`WorldCmd`]s to the async host
//! (`repl.rs::ReplHost`) and block on a std oneshot with
//! [`BRIDGE_TIMEOUT`] — the fetch-bridge idiom of the page runtime,
//! applied to a context we own. The page `JsRuntime` is untouched.
//!
//! Programs run as `(async () => { … })()` so top-level `await` works
//! (boa parses plain scripts without it). Playwright-shaped globals are
//! installed by a JS prelude over three natives (`__host`, `__sleep`,
//! `__console`).
//!
//! M1 binding list (the model-visible surface):
//! - `page`: goto/content/observe/ariaSnapshot/screenshot/url/title/
//!   getByRole/getByText/getByLabel/getByPlaceholder/locator/waitForTimeout
//! - locators: first/nth/click/fill/ref (+ explicit UNSUPPORTED throws for
//!   press/check — the error contract in action)
//! - `oxi`: fillCredential(locator, account)
//! - `console` (log/warn/error/info), `JSON`, `Date`, `Math` (boa builtins)
//! - `require`/`process`/`Buffer` throw UNSUPPORTED with guidance — the
//!   world is not Node; errors tell the model what to use instead.

use std::time::{Duration, Instant};

use boa_engine::{
    Context, JsError, JsNativeError, JsValue, NativeFunction, Source, context::ContextBuilder,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::locators::LocatorQuery;
use super::repl::{ActKind, BRIDGE_TIMEOUT, WorldCmd};

/// One program submission from the CLI loop.
struct Program {
    src: String,
    resp: std::sync::mpsc::Sender<Result<(Value, Option<Value>), String>>,
}

/// Handle to the world thread.
pub struct World {
    prog_tx: std::sync::mpsc::Sender<Program>,
}

impl World {
    /// Spawn the world thread. `cmd_tx` reaches the REPL host.
    pub fn spawn(cmd_tx: mpsc::Sender<WorldCmd>) -> Self {
        let (prog_tx, prog_rx) = std::sync::mpsc::channel::<Program>();
        std::thread::Builder::new()
            .name("oxibrowser-agent-world".into())
            .spawn(move || world_thread(prog_rx, cmd_tx))
            .expect("spawn agent world thread");
        Self { prog_tx }
    }

    /// Evaluate one program block; returns its JSON value plus the
    /// `task.done` payload when the program called it.
    pub fn eval(&self, src: &str) -> Result<(Value, Option<Value>), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.prog_tx
            .send(Program {
                src: src.to_string(),
                resp: tx,
            })
            .map_err(|_| "world thread died".to_string())?;
        rx.recv_timeout(BRIDGE_TIMEOUT + Duration::from_secs(30))
            .map_err(|_| "world did not answer".to_string())?
    }
}

fn world_thread(prog_rx: std::sync::mpsc::Receiver<Program>, cmd_tx: mpsc::Sender<WorldCmd>) {
    let mut ctx = ContextBuilder::default()
        .build()
        .expect("build agent-world context");
    register_natives(&mut ctx, cmd_tx);
    if let Err(e) = ctx.eval(Source::from_bytes(PRELUDE)) {
        tracing::error!(error = %e, "agent-world prelude failed");
    }
    for Program { src, resp } in prog_rx {
        let _ = resp.send(eval_program(&mut ctx, &src));
    }
}

fn js_err(msg: impl std::fmt::Display) -> JsError {
    JsError::from_native(JsNativeError::error().with_message(msg.to_string()))
}

fn arg_str(args: &[JsValue], ctx: &mut Context, i: usize) -> String {
    args.get(i)
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default()
}

fn register_natives(ctx: &mut Context, cmd_tx: mpsc::Sender<WorldCmd>) {
    // __host(kind, payloadJson) — the single bridge to the REPL host.
    let host_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let kind = arg_str(args, ctx, 0);
            let payload: Value =
                serde_json::from_str(&arg_str(args, ctx, 1)).unwrap_or_else(|_| json!({}));

            let (tx, rx) = std::sync::mpsc::channel();
            let cmd = match kind.as_str() {
                "waitEvent" => WorldCmd::WaitEvent {
                    kind: payload["kind"].as_str().unwrap_or("any").to_string(),
                    timeout_ms: payload["timeout_ms"].as_u64().unwrap_or(20_000),
                    resp: tx,
                },
                "mouseClick" => WorldCmd::MouseClick {
                    x: payload["x"].as_f64().unwrap_or(0.0),
                    y: payload["y"].as_f64().unwrap_or(0.0),
                    resp: tx,
                },
                "replay" => WorldCmd::Replay {
                    url: payload["url"].as_str().unwrap_or_default().to_string(),
                    resp: tx,
                },
                "login" => WorldCmd::Login {
                    account_id: payload["account"].as_str().unwrap_or_default().to_string(),
                    resp: tx,
                },
                "resolveDialog" => WorldCmd::ResolveDialog {
                    accept: payload["accept"].as_bool().unwrap_or(true),
                    prompt_text: payload["prompt_text"].as_str().map(str::to_string),
                    resp: tx,
                },
                "goto" => WorldCmd::Goto {
                    url: payload["url"].as_str().unwrap_or_default().to_string(),
                    resp: tx,
                },
                "content" => WorldCmd::Content { resp: tx },
                "meta" => WorldCmd::Meta { resp: tx },
                "screenshot" => WorldCmd::Screenshot { resp: tx },
                "observe" => WorldCmd::Observe {
                    budget_tokens: payload["budget_tokens"].as_u64(),
                    resp: tx,
                },
                "act" => {
                    let query: LocatorQuery = serde_json::from_value(payload["query"].clone())
                        .map_err(|e| js_err(format!("invalid locator: {e}")))?;
                    let action = match payload["action"]["kind"].as_str().unwrap_or("") {
                        "click" => ActKind::Click,
                        "fill" => ActKind::Fill(
                            payload["action"]["value"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        ),
                        "ref" => ActKind::RefOnly,
                        other => {
                            return Err(js_err(format!(
                                "[UNSUPPORTED] action {other:?} is not implemented"
                            )));
                        }
                    };
                    WorldCmd::Act {
                        query,
                        action,
                        resp: tx,
                    }
                }
                "fillCredential" => {
                    let query: LocatorQuery = serde_json::from_value(payload["query"].clone())
                        .map_err(|e| js_err(format!("invalid locator: {e}")))?;
                    WorldCmd::FillCredential {
                        query,
                        account_id: payload["account"].as_str().unwrap_or_default().to_string(),
                        resp: tx,
                    }
                }
                other => return Err(js_err(format!("[UNSUPPORTED] host call {other:?}"))),
            };

            cmd_tx
                .blocking_send(cmd)
                .map_err(|_| js_err("[BRIDGE_CLOSED] the host is gone"))?;
            match rx.recv_timeout(BRIDGE_TIMEOUT) {
                Ok(Ok(value)) => {
                    let js = JsValue::from_json(&value, ctx)
                        .map_err(|e| js_err(format!("bridge serialization: {e}")))?;
                    Ok(js)
                }
                Ok(Err(e)) => Err(js_err(format!(
                    "[{}] {}",
                    e["code"].as_str().unwrap_or("ERROR"),
                    e["message"].as_str().unwrap_or("")
                ))),
                Err(_) => Err(js_err(format!(
                    "[BRIDGE_TIMEOUT] host did not answer within {}s",
                    BRIDGE_TIMEOUT.as_secs()
                ))),
            }
        })
    };
    let _ = ctx.register_global_callable(boa_engine::js_string!("__host"), 2, host_fn);

    // __sleep(ms) — world-thread sleep (waitForTimeout must not block the
    // host, which other tabs and the viewer share).
    let sleep_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, _ctx| {
            let ms = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
            std::thread::sleep(Duration::from_millis(ms.max(0.0) as u64));
            Ok(JsValue::undefined())
        })
    };
    let _ = ctx.register_global_callable(boa_engine::js_string!("__sleep"), 1, sleep_fn);

    // __console(level, text) — stderr transcript line.
    let console_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let level = arg_str(args, ctx, 0);
            let text = arg_str(args, ctx, 1);
            eprintln!("[repl:{level}] {text}");
            Ok(JsValue::undefined())
        })
    };
    let _ = ctx.register_global_callable(boa_engine::js_string!("__console"), 2, console_fn);
}

/// Evaluate a program: async-IIFE wrap → settle via promise handlers →
/// poll the job queue until the 60s guard trips. Returns the program value
/// plus `task.done`'s payload when the program called it.
fn eval_program(ctx: &mut Context, src: &str) -> Result<(Value, Option<Value>), String> {
    use boa_engine::property::Attribute;

    let wrapped = format!("(async () => {{\n{src}\n}})()");
    let promise = ctx
        .eval(Source::from_bytes(&wrapped))
        .map_err(|e| format!("{e}"))?;

    let _ =
        ctx.register_global_property(boa_engine::js_string!("__replP"), promise, Attribute::all());
    ctx.eval(Source::from_bytes(
        r#"globalThis.__replSettled = null;
Promise.resolve(globalThis.__replP).then(
  v => { globalThis.__replSettled = { ok: true, value: v }; },
  e => { globalThis.__replSettled = { ok: false, error: String(e) }; }
); null"#,
    ))
    .map_err(|e| format!("settle setup failed: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        ctx.run_jobs()
            .map_err(|e| format!("promise job failed: {e}"))?;
        let settled = ctx
            .eval(Source::from_bytes("globalThis.__replSettled"))
            .map_err(|e| format!("settle read failed: {e}"))?;
        if !settled.is_null_or_undefined() {
            let obj = settled
                .as_object()
                .ok_or_else(|| "settle marker is not an object".to_string())?;
            let ok = obj
                .get(boa_engine::js_string!("ok"), ctx)
                .map_err(|e| format!("{e}"))?
                .to_boolean();
            let field = if ok { "value" } else { "error" };
            let v = obj
                .get(boa_engine::js_string!(field), ctx)
                .map_err(|e| format!("{e}"))?;
            // best-effort cleanup of the stashed globals
            let _ = ctx.eval(Source::from_bytes(
                "delete globalThis.__replP; delete globalThis.__replSettled;",
            ));
            let done = ctx
                .eval(Source::from_bytes(
                    "(function (v) { delete globalThis.__taskDone; return v; })(globalThis.__taskDone === undefined ? null : globalThis.__taskDone)",
                ))
                .ok()
                .and_then(|v| {
                    if v.is_null_or_undefined() {
                        None
                    } else {
                        v.to_json(ctx).ok().flatten()
                    }
                });
            return if ok {
                let value = if v.is_undefined() {
                    // Programs without an explicit `return` settle to
                    // undefined; boa's to_json panics on it.
                    Value::Null
                } else {
                    // boa 0.22 to_json wraps in Option (undefined → None).
                    v.to_json(ctx)
                        .map_err(|e| format!("result serialization: {e}"))?
                        .unwrap_or(Value::Null)
                };
                Ok((value, done))
            } else {
                Err(v
                    .to_string(ctx)
                    .map_err(|e| format!("{e}"))?
                    .to_std_string_escaped())
            };
        }
        if Instant::now() > deadline {
            return Err("[REPL_TIMEOUT] program did not settle within 60s".to_string());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The Playwright-shaped surface. Kept in JS: chains, classes, and error
/// messages live where the model can read them back.
const PRELUDE: &str = r#"

class Dialog {
  constructor(d) { Object.assign(this, d); }
  accept(text) {
    return __host('resolveDialog', JSON.stringify({
      accept: true, prompt_text: text != null ? String(text) : null,
    }));
  }
  dismiss() {
    return __host('resolveDialog', JSON.stringify({ accept: false, prompt_text: null }));
  }
  toString() { return this.dialog_type + ': ' + this.message; }
}
'use strict';
globalThis.task = {
  done: (result) => { globalThis.__taskDone = result === undefined ? null : result; return result; },
};
class Locator {
  constructor(q) { this.__q = q; }
  first() { return new Locator(Object.assign({}, this.__q, { nth: 0 })); }
  nth(i) { return new Locator(Object.assign({}, this.__q, { nth: i })); }
  async click() { return __host('act', JSON.stringify({ query: this.__q, action: { kind: 'click' } })); }
  async fill(v) { return __host('act', JSON.stringify({ query: this.__q, action: { kind: 'fill', value: String(v) } })); }
  async ref() { return __host('act', JSON.stringify({ query: this.__q, action: { kind: 'ref' } })); }
  async press() { throw new Error('[UNSUPPORTED] key press is not implemented in the native input path yet'); }
  async check() { throw new Error('[UNSUPPORTED] checkbox toggle is not implemented in the native input path yet'); }
  toString() { return 'locator(' + JSON.stringify(this.__q) + ')'; }
}
function spec(method, value) { return { method: method, value: value }; }
globalThis.page = {
  async goto(u) { return __host('goto', JSON.stringify({ url: String(u) })); },
  async content() { return __host('content', '{}'); },
  async observe(o) {
    const budget = (o && (o.budgetTokens ?? o.budget_tokens)) || null;
    return __host('observe', JSON.stringify({ budget_tokens: budget }));
  },
  async ariaSnapshot(o) { return (await this.observe(o)).aria_yaml; },
  async screenshot() { return __host('screenshot', '{}'); },
  async url() { return (await __host('meta', '{}')).url; },
  async title() { return (await __host('meta', '{}')).title; },
  async waitForTimeout(ms) { return __sleep(Number(ms) || 0); },
  async login(account) { return __host('login', JSON.stringify({ account: String(account) })); },
  get mouse() {
    return {
      click: (x, y) => __host('mouseClick', JSON.stringify({ x: Number(x), y: Number(y) })),
    };
  },
  async replay(o) { return __host('replay', JSON.stringify({ url: o == null ? '' : String((typeof o === 'string') ? o : (o.url || '')) })); },
  async waitForEvent(kind, opts) {
    const ev = await __host('waitEvent', JSON.stringify({
      kind: String(kind),
      timeout_ms: (opts && (opts.timeoutMs ?? opts.timeout_ms)) || 20000,
    }));
    return (kind === 'dialog' && ev && ev.type === 'dialog') ? new Dialog(ev) : ev;
  },
  getByRole(role, o) {
    return new Locator({ spec: spec('role', { role: String(role), name: (o && o.name != null) ? String(o.name) : null }), nth: null });
  },
  getByText(t) { return new Locator({ spec: spec('text', { text: String(t) }), nth: null }); },
  getByLabel(t) { return new Locator({ spec: spec('label', { text: String(t) }), nth: null }); },
  getByPlaceholder(t) { return new Locator({ spec: spec('placeholder', { text: String(t) }), nth: null }); },
  locator(css) { return new Locator({ spec: spec('css', { css: String(css) }), nth: null }); },
};
globalThis.oxi = {
  async fillCredential(loc, account) {
    return __host('fillCredential', JSON.stringify({ query: loc.__q, account: String(account) }));
  },
};
globalThis.console = {
  log: (...a) => __console('log', a.map(String).join(' ')),
  info: (...a) => __console('info', a.map(String).join(' ')),
  warn: (...a) => __console('warn', a.map(String).join(' ')),
  error: (...a) => __console('error', a.map(String).join(' ')),
};
// Node-trained models get a self-correcting error instead of a silent
// ReferenceError.
globalThis.require = () => { throw new Error('[UNSUPPORTED] require is not available in agent-world — browser surface is `page`/`oxi`'); };
globalThis.process = undefined;
globalThis.Buffer = undefined;
"#;
