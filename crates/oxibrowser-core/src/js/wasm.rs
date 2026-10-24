//! WebAssembly support — `wasmi` ↔ `boa_engine` bridge.
//!
//! Registers the `WebAssembly.*` global namespace so pages using WASM modules
//! (compile, instantiate, call exports, host-function imports, linear memory,
//! tables) work the same as in a real browser.
//!
//! ## Threading
//!
//! All WASM compilation and execution is **synchronous on the JS thread**, the
//! same thread that owns the `boa_engine::Context`. `wasmi` handles (`Engine`,
//! `Module`, `Store`, `Instance`) are not `Send`, so they never leave this
//! thread — exactly mirroring the boa runtime's `!Send` constraint.
//!
//! ## State model
//!
//! Compiled modules, live instances, and standalone memories/tables live in
//! thread-local registries keyed by a `u64` handle. The corresponding JS object
//! carries the handle in a hidden `__wasm_*` property. This matches the
//! codebase's `LISTENER_REGISTRY` pattern and avoids passing non-`Send` wasmi
//! state through boa's `NativeObjectData`.
//!
//! Host-function imports reach the active `Context` via a thread-local raw
//! pointer (`WASM_HOST_CTX`), set for the duration of an export call.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr;
use std::rc::Rc;

use boa_engine::object::builtins::JsArray;
use boa_engine::object::{FunctionObjectBuilder, JsObject};
use boa_engine::{
    JsArgs, JsError, JsNativeError, JsResult, JsValue, NativeFunction, Source, js_string,
};

// ── Thread-local state ──────────────────────────────────────────────────────

// Per-JS-thread wasmi engine. Fuel metering is enabled so an infinite WASM
// loop traps ([`WASM_FUEL_BUDGET`] instructions) instead of hanging the JS
// thread — matching how real browsers terminate runaway WASM.
thread_local! {
    static WASM_ENGINE: wasmi::Engine = make_engine();
}

/// Instruction budget per WASM execution. At ~10⁸ instr/s this caps a runaway
/// loop at well under a second before it traps as a `RuntimeError`.
const WASM_FUEL_BUDGET: u64 = 10_000_000;

/// Build the shared wasmi engine with fuel consumption enabled.
fn make_engine() -> wasmi::Engine {
    let mut cfg = wasmi::Config::default();
    cfg.consume_fuel(true);
    wasmi::Engine::new(&cfg)
}

/// Create a fuelled store: a fresh `Store` against the shared engine with the
/// default instruction budget installed. Every WASM execution draws from this.
fn wasm_store() -> wasmi::Store<WasmHostState> {
    let mut store = WASM_ENGINE.with(|eng| wasmi::Store::new(eng, WasmHostState));
    // consume_fuel is on, so set_fuel succeeds; on the off chance it fails we
    // proceed budget-less (compile/validate still work; only runaway loops lack
    // the guard).
    let _ = store.set_fuel(WASM_FUEL_BUDGET);
    store
}

thread_local! {
    /// `WebAssembly.Module` JS objects → compiled `wasmi::Module` (shared via `Rc`).
    static WASM_MODULES: RefCell<HashMap<u64, Rc<wasmi::Module>>> = RefCell::new(HashMap::new());

    /// `WebAssembly.Instance` JS objects → live instance owning a `Store`.
    static WASM_INSTANCES: RefCell<HashMap<u64, Rc<RefCell<WasmInstance>>>> =
        RefCell::new(HashMap::new());

    /// `new WebAssembly.Memory(...)` (not tied to an instance) → owning store + memory.
    static WASM_MEMORIES: RefCell<HashMap<u64, Rc<RefCell<WasmMemory>>>> =
        RefCell::new(HashMap::new());

    /// `new WebAssembly.Table(...)` (not tied to an instance) → owning store + table.
    static WASM_TABLES: RefCell<HashMap<u64, Rc<RefCell<WasmTable>>>> =
        RefCell::new(HashMap::new());

    /// JS callables backing imported host functions, keyed by the host-fn id
    /// captured in the wasmi closure. Stored as `JsValue` so they stay
    /// GC-rooted (mirrors `LISTENER_REGISTRY`).
    static HOST_FUNCS: RefCell<HashMap<u64, JsValue>> = RefCell::new(HashMap::new());

    static NEXT_WASM_ID: Cell<u64> = const { Cell::new(1) };
}

// Raw pointer to the active boa `Context`, valid only while an exported WASM
// function is executing on this thread. Host-function imports read it to call
// back into JS.
//
// # Safety
//
// Only the single JS thread ever accesses this cell. It is set immediately
// before an export call and cleared immediately after (including on trap), so
// there is never more than one `&mut Context` derived from it at a time — the
// export closure transfers its `&mut Context` "ownership" to host fns for the
// call's duration. `Context` is `!Send`, so the pointer never crosses threads.
thread_local! {
    static WASM_HOST_CTX: Cell<*mut boa_engine::Context> = const { Cell::new(ptr::null_mut()) };
}

// ── Handle types ────────────────────────────────────────────────────────────

/// Host state stored in every wasmi `Store`. Unused today (the host `Context`
/// is reached via `WASM_HOST_CTX`); required by the `Store<T>` type parameter.
#[derive(Default)]
pub struct WasmHostState;

/// A live instantiated WASM module: the owning `Store` plus the instance handle.
struct WasmInstance {
    store: wasmi::Store<WasmHostState>,
    instance: wasmi::Instance,
}

/// A standalone linear memory (from `new WebAssembly.Memory`), owning its store.
struct WasmMemory {
    store: wasmi::Store<WasmHostState>,
    memory: wasmi::Memory,
}

/// A standalone funcref table (from `new WebAssembly.Table`), owning its store.
struct WasmTable {
    // Kept alive so the `Table` handle remains valid; not read after construction.
    #[allow(dead_code)]
    store: wasmi::Store<WasmHostState>,
    table: wasmi::Table,
}

// ── Small helpers ───────────────────────────────────────────────────────────

const WASM_PAGE_SIZE: u64 = 65_536;

fn next_id() -> u64 {
    NEXT_WASM_ID.with(|c| {
        let id = c.get();
        c.set(id + 1);
        id
    })
}

/// Build a `TypeError` from any message.
fn type_err<S: Into<String>>(msg: S) -> JsError {
    JsNativeError::typ().with_message(msg.into()).into()
}

/// Read a `u64` handle stored as an f64 under `key` on a JS object.
fn get_handle(obj: &JsObject, key: &str, ctx: &mut boa_engine::Context) -> Option<u64> {
    obj.get(js_string!(key), ctx)
        .ok()
        .and_then(|v| v.as_number().map(|n| n as u64))
}

/// Look up a WASM error constructor on the `WebAssembly` namespace.
fn wasm_error_ctor(kind: &str, ctx: &mut boa_engine::Context) -> Option<JsObject> {
    let wasm = ctx
        .global_object()
        .get(js_string!("WebAssembly"), ctx)
        .ok()?;
    wasm.as_object()?
        .get(js_string!(kind), ctx)
        .ok()?
        .as_object()
}

/// Mint a fresh empty JS object with the default object prototype.
fn new_object(ctx: &mut boa_engine::Context) -> JsObject {
    JsObject::with_object_proto(ctx.intrinsics())
}

/// Wrap a `NativeFunction` into a JS `JsValue` (a function object built in the
/// active realm). Used for methods attached to namespace objects.
fn native_fn_to_value(f: NativeFunction, ctx: &mut boa_engine::Context) -> JsValue {
    JsValue::from(FunctionObjectBuilder::new(ctx.realm(), f).build())
}

/// Extract a byte buffer from a JS value (Uint8Array / ArrayBuffer / Array of
/// numbers). Mirrors `extract_binary_bytes` in `runtime.rs`.
fn extract_bytes(v: &JsValue, ctx: &mut boa_engine::Context) -> Result<Vec<u8>, JsError> {
    let obj = v
        .as_object()
        .ok_or_else(|| type_err("expected a BufferSource (Uint8Array/Array)"))?;
    let len = obj
        .get(js_string!("length"), ctx)?
        .as_number()
        .or_else(|| {
            obj.get(js_string!("byteLength"), ctx)
                .ok()
                .and_then(|l| l.as_number())
        })
        .ok_or_else(|| type_err("buffer has no length/byteLength"))?;
    let n = len as usize;
    let mut bytes = Vec::with_capacity(n);
    for i in 0..n {
        let b = obj.get(i as u32, ctx)?.to_number(ctx)? as u8;
        bytes.push(b);
    }
    Ok(bytes)
}

fn to_u32(v: &JsValue, ctx: &mut boa_engine::Context) -> Result<u32, JsError> {
    Ok(v.to_number(ctx)? as u32)
}

/// Construct a WASM error object and return it as a throwable `JsError`.
fn wasm_throw(ctor: &JsObject, msg: &str, ctx: &mut boa_engine::Context) -> JsResult<JsValue> {
    let err = ctor.construct(&[JsValue::from(js_string!(msg))], None, ctx)?;
    Err(JsError::from_opaque(JsValue::from(err)))
}

// ── Value bridge ────────────────────────────────────────────────────────────

/// Convert a wasmi value into a JS value. `i64` uses a JS `Number` (exact up to
/// ±2⁵³); reference/V128 types fall back to `undefined` (not part of the MVP
/// JS surface).
fn val_to_jsval(val: &wasmi::Val) -> JsValue {
    match val {
        wasmi::Val::I32(i) => JsValue::from(*i),
        wasmi::Val::I64(i) => JsValue::from(*i as f64),
        wasmi::Val::F32(f) => JsValue::from(f.to_float()),
        wasmi::Val::F64(f) => JsValue::from(f.to_float()),
        // funcref/externref/v128: not exposed on the JS side for MVP.
        _ => JsValue::undefined(),
    }
}

/// Convert a JS value into a wasmi value of the requested type. Errors are
/// returned as a human-readable string (callers decide how to surface them).
fn jsval_to_val(
    v: &JsValue,
    ty: wasmi::ValType,
    ctx: &mut boa_engine::Context,
) -> Result<wasmi::Val, String> {
    match ty {
        wasmi::ValType::I32 => Ok(wasmi::Val::I32(v.to_i32(ctx).map_err(|e| e.to_string())?)),
        wasmi::ValType::I64 => Ok(wasmi::Val::I64(
            v.to_number(ctx).map_err(|e| e.to_string())? as i64,
        )),
        wasmi::ValType::F32 => Ok(wasmi::Val::F32(
            (v.to_number(ctx).map_err(|e| e.to_string())? as f32).into(),
        )),
        wasmi::ValType::F64 => Ok(wasmi::Val::F64(
            v.to_number(ctx).map_err(|e| e.to_string())?.into(),
        )),
        // Reference / V128 types: pass a default value (no JS binding yet).
        _ => Ok(wasmi::Val::default(ty)),
    }
}

// ── Bootstrap ───────────────────────────────────────────────────────────────

/// Installs the three WASM error subclasses on `globalThis.__wasmErrors`. The
/// thrown instances carry the right prototype chain so `e instanceof
/// WebAssembly.CompileError` holds.
const ERRORS_BOOTSTRAP: &str = r#"
(function () {
  function makeError(name) {
    function Ctor(msg) {
      var m = (msg === undefined) ? "" : String(msg);
      var e = new Error(m);
      e.name = name;
      e.message = m;
      Object.setPrototypeOf(e, Ctor.prototype);
      return e;
    }
    Ctor.prototype = Object.create(Error.prototype);
    Object.defineProperty(Ctor.prototype, "constructor", { value: Ctor });
    Object.defineProperty(Ctor.prototype, "name", { value: name });
    return Ctor;
  }
  globalThis.__wasmErrors = {
    CompileError: makeError("CompileError"),
    LinkError: makeError("LinkError"),
    RuntimeError: makeError("RuntimeError")
  };
})();
"#;

/// Assembles the `WebAssembly` global namespace from the native `__wasm*`
/// callables, then deletes the temporary helpers. `compile`/`instantiate` are
/// expressed in JS over `Module`/`Instance`, returning `Promise`s.
const WASM_BOOTSTRAP: &str = r#"
globalThis.WebAssembly = {
  validate: __wasmValidate,
  Module: __wasmModule,
  Instance: __wasmInstance,
  Memory: __wasmMemory,
  Table: __wasmTable,
  CompileError: __wasmErrors.CompileError,
  LinkError: __wasmErrors.LinkError,
  RuntimeError: __wasmErrors.RuntimeError,
  compile: function (bytes) {
    return Promise.resolve(new WebAssembly.Module(bytes));
  },
  instantiate: function (source, imports) {
    var mod = (source && typeof source === "object"
               && Object.prototype.hasOwnProperty.call(source, "__wasm_module"))
      ? source
      : new WebAssembly.Module(source);
    return Promise.resolve(new WebAssembly.Instance(mod, imports));
  }
};
delete globalThis.__wasmValidate;
delete globalThis.__wasmModule;
delete globalThis.__wasmInstance;
delete globalThis.__wasmMemory;
delete globalThis.__wasmTable;
delete globalThis.__wasmErrors;
"#;

// ── Registration ────────────────────────────────────────────────────────────

/// Register all `WebAssembly.*` globals on the given boa `Context`.
///
/// Installs the `WebAssembly` namespace with `validate`, `compile`,
/// `instantiate`, `Module`, `Instance`, `Memory`, `Table`, and the three error
/// constructors. Called once from `create_context` during JS-thread setup.
pub fn register_wasm_globals(ctx: &mut boa_engine::Context) -> JsResult<()> {
    let _ = ctx.eval(Source::from_bytes(ERRORS_BOOTSTRAP));

    // --- WebAssembly.validate(bytes) → boolean ---
    let validate_fn = NativeFunction::from_fn_ptr(|_this, args, ctx| {
        let bytes = extract_bytes(args.get_or_undefined(0), ctx).unwrap_or_default();
        let ok = WASM_ENGINE.with(|eng| wasmi::Module::validate(eng, &bytes).is_ok());
        Ok(JsValue::from(ok))
    });
    ctx.register_global_callable(js_string!("__wasmValidate"), 1, validate_fn)?;

    // --- WebAssembly.Module(bytes) ---
    let module_ctor = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let bytes = extract_bytes(args.get_or_undefined(0), ctx).unwrap_or_default();
            match WASM_ENGINE.with(|eng| wasmi::Module::new(eng, &bytes)) {
                Ok(m) => {
                    let id = next_id();
                    WASM_MODULES.with(|reg| reg.borrow_mut().insert(id, Rc::new(m)));
                    let obj = new_object(ctx);
                    obj.set(
                        js_string!("__wasm_module"),
                        JsValue::from(id as f64),
                        true,
                        ctx,
                    )?;
                    Ok(JsValue::from(obj))
                }
                Err(e) => match wasm_error_ctor("CompileError", ctx) {
                    Some(c) => wasm_throw(&c, &e.to_string(), ctx),
                    None => Err(type_err(e.to_string())),
                },
            }
        })
    };
    ctx.register_global_callable(js_string!("__wasmModule"), 1, module_ctor)?;

    // --- WebAssembly.Instance(module, imports) ---
    let instance_ctor =
        unsafe { NativeFunction::from_closure(move |_this, args, ctx| build_instance(args, ctx)) };
    ctx.register_global_callable(js_string!("__wasmInstance"), 2, instance_ctor)?;

    // --- WebAssembly.Memory(descriptor) ---
    let memory_ctor =
        unsafe { NativeFunction::from_closure(move |_this, args, ctx| build_memory(args, ctx)) };
    ctx.register_global_callable(js_string!("__wasmMemory"), 1, memory_ctor)?;

    // --- WebAssembly.Table(descriptor) ---
    let table_ctor =
        unsafe { NativeFunction::from_closure(move |_this, args, ctx| build_table(args, ctx)) };
    ctx.register_global_callable(js_string!("__wasmTable"), 1, table_ctor)?;

    ctx.eval(Source::from_bytes(WASM_BOOTSTRAP))?;
    Ok(())
}

// ── Instance construction + exports ─────────────────────────────────────────

fn build_instance(args: &[JsValue], ctx: &mut boa_engine::Context) -> JsResult<JsValue> {
    let module_obj = args
        .get_or_undefined(0)
        .as_object()
        .ok_or_else(|| type_err("expected a WebAssembly.Module"))?;
    let module_id = get_handle(&module_obj, "__wasm_module", ctx)
        .ok_or_else(|| type_err("invalid WebAssembly.Module"))?;
    let module = WASM_MODULES
        .with(|reg| reg.borrow().get(&module_id).cloned())
        .ok_or_else(|| type_err("stale WebAssembly.Module handle"))?;

    let store = wasm_store();
    let linker = WASM_ENGINE.with(wasmi::Linker::<WasmHostState>::new);
    let import_obj = args.get_or_undefined(1).clone();
    let linker = resolve_imports(linker, &module, &import_obj, ctx)?;

    // Instantiate into the store, then wrap store + instance together so export
    // closures can share them via `Rc<RefCell<…>>`.
    let mut store = store;
    let instance = match linker.instantiate_and_start(&mut store, &module) {
        Ok(inst) => inst,
        Err(e) => {
            return match wasm_error_ctor("LinkError", ctx) {
                Some(l) => wasm_throw(&l, &e.to_string(), ctx),
                None => Err(type_err(e.to_string())),
            };
        }
    };
    let inst_rc = Rc::new(RefCell::new(WasmInstance { store, instance }));

    // Build the `exports` object from the instance's named exports.
    let exports_obj = new_object(ctx);
    let export_names: Vec<String> = {
        let g = inst_rc.borrow();
        g.instance
            .exports(&g.store)
            .map(|e| e.name().to_string())
            .collect()
    };
    for name in export_names {
        let (func, mem, tab) = {
            let g = inst_rc.borrow();
            match g.instance.get_export(&g.store, &name) {
                Some(wasmi::Extern::Func(f)) => {
                    let ft = f.ty(&g.store);
                    (Some((f, ft)), None, None)
                }
                Some(wasmi::Extern::Memory(m)) => (None, Some(m), None),
                Some(wasmi::Extern::Table(t)) => (None, None, Some(t)),
                _ => (None, None, None),
            }
        };
        if let Some((func, ft)) = func {
            let param_types = ft.params().to_vec();
            let result_types = ft.results().to_vec();
            let callable =
                make_export_callable(inst_rc.clone(), func, param_types, result_types, ctx)?;
            exports_obj.set(
                js_string!(name.as_str()),
                JsValue::from(callable),
                true,
                ctx,
            )?;
        } else if let Some(mem) = mem {
            let mobj = make_exported_memory_object(inst_rc.clone(), mem, ctx)?;
            exports_obj.set(js_string!(name.as_str()), JsValue::from(mobj), true, ctx)?;
        } else if let Some(tab) = tab {
            let tobj = make_exported_table_object(inst_rc.clone(), tab, ctx)?;
            exports_obj.set(js_string!(name.as_str()), JsValue::from(tobj), true, ctx)?;
        }
    }

    let id = next_id();
    WASM_INSTANCES.with(|reg| reg.borrow_mut().insert(id, inst_rc));
    let obj = new_object(ctx);
    obj.set(
        js_string!("__wasm_instance"),
        JsValue::from(id as f64),
        true,
        ctx,
    )?;
    obj.set(js_string!("exports"), JsValue::from(exports_obj), true, ctx)?;
    Ok(JsValue::from(obj))
}

/// Build a callable JS function wrapping a WASM exported `Func`.
fn make_export_callable(
    inst_rc: Rc<RefCell<WasmInstance>>,
    func: wasmi::Func,
    param_types: Vec<wasmi::ValType>,
    result_types: Vec<wasmi::ValType>,
    ctx: &mut boa_engine::Context,
) -> JsResult<JsObject> {
    let callable = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            // Convert JS args → wasmi Vals.
            let inputs: Vec<wasmi::Val> = param_types
                .iter()
                .enumerate()
                .map(|(i, ty)| jsval_to_val(args.get_or_undefined(i), *ty, ctx))
                .collect::<Result<_, _>>()
                .map_err(type_err)?;

            let mut outputs: Vec<wasmi::Val> = result_types
                .iter()
                .map(|t| wasmi::Val::default(*t))
                .collect();

            // Expose the active Context to host-function imports for this call.
            let ctx_ptr: *mut boa_engine::Context = ctx;
            WASM_HOST_CTX.with(|c| c.set(ctx_ptr));

            // Borrow the store mutably and invoke. A host fn that re-enters WASM
            // would double-borrow; surface that as a trap instead of panicking.
            let call_result = match inst_rc.try_borrow_mut() {
                Ok(mut winst) => func.call(&mut winst.store, &inputs, &mut outputs),
                Err(_) => Err(wasmi::Error::new(
                    "WASM re-entrant call during host function (not supported)",
                )),
            };

            WASM_HOST_CTX.with(|c| c.set(ptr::null_mut()));

            match call_result {
                Ok(()) => {
                    if outputs.is_empty() {
                        Ok(JsValue::undefined())
                    } else if outputs.len() == 1 {
                        Ok(val_to_jsval(&outputs[0]))
                    } else {
                        let arr = JsArray::new(ctx).expect(
                            "creating an empty array with the default prototype must not fail",
                        );
                        for v in &outputs {
                            arr.push(val_to_jsval(v), ctx)?;
                        }
                        Ok(JsValue::from(arr))
                    }
                }
                Err(e) => match wasm_error_ctor("RuntimeError", ctx) {
                    Some(r) => wasm_throw(&r, &e.to_string(), ctx),
                    None => Err(type_err(e.to_string())),
                },
            }
        })
    };
    Ok(JsObject::from(
        FunctionObjectBuilder::new(ctx.realm(), callable).build(),
    ))
}

/// Resolve the JS import object into wasmi linker definitions.
///
/// For each declared module import, look up `imports[module][name]`:
/// - function imports → register a host function that calls back into JS;
/// - memory/table imports → pass the provided WebAssembly handle through.
///
/// Global imports are not yet supported (MVP gap).
fn resolve_imports(
    mut linker: wasmi::Linker<WasmHostState>,
    module: &wasmi::Module,
    import_obj: &JsValue,
    ctx: &mut boa_engine::Context,
) -> JsResult<wasmi::Linker<WasmHostState>> {
    let import_root = import_obj.as_object();
    for imp in module.imports() {
        let module_name = imp.module();
        let field_name = imp.name();
        let val = match &import_root {
            Some(root) => root
                .get(js_string!(module_name), ctx)
                .ok()
                .and_then(|ns| ns.as_object()?.get(js_string!(field_name), ctx).ok()),
            None => None,
        };

        match imp.ty() {
            wasmi::ExternType::Func(ft) => {
                let Some(v) = &val else {
                    // No import provided — leave it; instantiation traps with LinkError.
                    continue;
                };
                let result_types: Vec<wasmi::ValType> = ft.results().to_vec();
                let host_id = next_id();
                HOST_FUNCS.with(|reg| reg.borrow_mut().insert(host_id, v.clone()));
                linker
                    .func_new(
                        module_name,
                        field_name,
                        ft.clone(),
                        move |_caller, args, results| {
                            host_func_body(host_id, &result_types, args, results)
                        },
                    )
                    .map_err(|e| type_err(e.to_string()))?;
            }
            wasmi::ExternType::Memory(_) => {
                if let Some(o) = val.as_ref().and_then(|v| v.as_object())
                    && let Some(id) = get_handle(&o, "__wasm_memory", ctx)
                {
                    let mem = WASM_MEMORIES
                        .with(|reg| reg.borrow().get(&id).cloned())
                        .ok_or_else(|| type_err("stale memory import"))?;
                    linker
                        .define(module_name, field_name, mem.borrow().memory)
                        .map_err(|e| type_err(e.to_string()))?;
                }
            }
            wasmi::ExternType::Table(_) => {
                if let Some(o) = val.as_ref().and_then(|v| v.as_object())
                    && let Some(id) = get_handle(&o, "__wasm_table", ctx)
                {
                    let tab = WASM_TABLES
                        .with(|reg| reg.borrow().get(&id).cloned())
                        .ok_or_else(|| type_err("stale table import"))?;
                    linker
                        .define(module_name, field_name, tab.borrow().table)
                        .map_err(|e| type_err(e.to_string()))?;
                }
            }
            wasmi::ExternType::Global(_) => {
                // Global imports require a store-owned `Global`, which the
                // linker cannot create without a store context. Deferred.
            }
        }
    }
    Ok(linker)
}

/// Host-function body shared by all imported functions: look up the JS callable
/// by `host_id`, convert wasmi args → JS, call it, convert the return → wasmi.
fn host_func_body(
    host_id: u64,
    result_types: &[wasmi::ValType],
    args: &[wasmi::Val],
    results: &mut [wasmi::Val],
) -> Result<(), wasmi::Error> {
    let ctx_ptr = WASM_HOST_CTX.with(|c| c.get());
    if ctx_ptr.is_null() {
        return Err(wasmi::Error::new(
            "WASM host function called without an active JS context",
        ));
    }
    // SAFETY: the export-call closure sets this pointer to a live `&mut Context`
    // for the duration of the call and clears it afterwards.
    let ctx: &mut boa_engine::Context = unsafe { &mut *ctx_ptr };

    // Clone the callable out so the registry borrow is released before JS re-entry.
    let callable = HOST_FUNCS
        .with(|reg| reg.borrow().get(&host_id).cloned())
        .ok_or_else(|| wasmi::Error::new("missing WASM host function import"))?;
    let js_callable = callable
        .as_callable()
        .ok_or_else(|| wasmi::Error::new("WASM host import is not callable"))?;

    let js_args: Vec<JsValue> = args.iter().map(val_to_jsval).collect();
    let ret = js_callable
        .call(&JsValue::undefined(), &js_args, ctx)
        .map_err(|e| wasmi::Error::new(e.to_string()))?;

    if let Some(rty) = result_types.first() {
        results[0] = jsval_to_val(&ret, *rty, ctx).map_err(wasmi::Error::new)?;
    }
    Ok(())
}

// ── Standalone Memory ───────────────────────────────────────────────────────

fn build_memory(args: &[JsValue], ctx: &mut boa_engine::Context) -> JsResult<JsValue> {
    let desc = args
        .get_or_undefined(0)
        .as_object()
        .ok_or_else(|| type_err("WebAssembly.Memory requires a descriptor"))?;
    let initial = to_u32(&desc.get(js_string!("initial"), ctx)?, ctx)?;
    let maximum = desc
        .get(js_string!("maximum"), ctx)
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u32);

    let mut store = wasm_store();
    let mty = wasmi::MemoryType::new(initial, maximum);
    let memory = wasmi::Memory::new(&mut store, mty).map_err(|e| type_err(e.to_string()))?;
    let id = next_id();
    let wm = Rc::new(RefCell::new(WasmMemory { store, memory }));
    let bytes = memory_size(&wm);
    WASM_MEMORIES.with(|reg| reg.borrow_mut().insert(id, wm));

    let mobj = new_object(ctx);
    mobj.set(
        js_string!("__wasm_memory"),
        JsValue::from(id as f64),
        true,
        ctx,
    )?;
    set_buffer_view(&mobj, bytes, ctx)?;

    let id_for_grow = id;
    let grow_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let delta = to_u32(args.get_or_undefined(0), ctx)?;
            let mem = WASM_MEMORIES
                .with(|reg| reg.borrow().get(&id_for_grow).cloned())
                .ok_or_else(|| type_err("stale WebAssembly.Memory"))?;
            let old = {
                let mut wm = mem.borrow_mut();
                let memory = wm.memory;
                memory.grow(&mut wm.store, delta as u64)
            };
            match old {
                Ok(old_pages) => {
                    // Refresh the `.buffer` snapshot to the new size.
                    let new_bytes = memory_size(&mem);
                    if let Some(this_obj) = _this.as_object() {
                        set_buffer_view(&this_obj, new_bytes, ctx)?;
                    }
                    Ok(JsValue::from(old_pages))
                }
                Err(e) => Err(JsNativeError::range().with_message(e.to_string()).into()),
            }
        })
    };
    mobj.set(
        js_string!("grow"),
        native_fn_to_value(grow_fn, ctx),
        false,
        ctx,
    )?;
    Ok(JsValue::from(mobj))
}

/// Current byte length of a standalone memory (pages × 64KiB).
fn memory_size(wm: &Rc<RefCell<WasmMemory>>) -> u64 {
    let g = wm.borrow();
    g.memory.size(&g.store) * WASM_PAGE_SIZE
}

/// Set/refresh a `buffer` property on a memory object: a lightweight view with a
/// `byteLength` snapshot. boa 0.20 lacks a real `ArrayBuffer`, so this mirrors
/// the Canvas shim's plain-object approach.
fn set_buffer_view(mem_obj: &JsObject, bytes: u64, ctx: &mut boa_engine::Context) -> JsResult<()> {
    let view = new_object(ctx);
    view.set(
        js_string!("byteLength"),
        JsValue::from(bytes as f64),
        false,
        ctx,
    )?;
    mem_obj.set(js_string!("buffer"), JsValue::from(view), true, ctx)?;
    Ok(())
}

/// Bind an instance-exported memory as a WebAssembly.Memory-like object with a
/// `buffer.byteLength` reflecting the instance store.
fn make_exported_memory_object(
    inst_rc: Rc<RefCell<WasmInstance>>,
    mem: wasmi::Memory,
    ctx: &mut boa_engine::Context,
) -> JsResult<JsObject> {
    let mobj = new_object(ctx);
    let bytes = {
        let g = inst_rc.borrow();
        mem.size(&g.store) * WASM_PAGE_SIZE
    };
    set_buffer_view(&mobj, bytes, ctx)?;
    Ok(mobj)
}

/// Bind an instance-exported table as a WebAssembly.Table-like object with a
/// `length` reflecting the instance store.
fn make_exported_table_object(
    inst_rc: Rc<RefCell<WasmInstance>>,
    tab: wasmi::Table,
    ctx: &mut boa_engine::Context,
) -> JsResult<JsObject> {
    let tobj = new_object(ctx);
    let len = {
        let g = inst_rc.borrow();
        tab.size(&g.store)
    };
    tobj.set(js_string!("length"), JsValue::from(len as f64), false, ctx)?;
    Ok(tobj)
}

// ── Standalone Table ────────────────────────────────────────────────────────

fn build_table(args: &[JsValue], ctx: &mut boa_engine::Context) -> JsResult<JsValue> {
    let desc = args
        .get_or_undefined(0)
        .as_object()
        .ok_or_else(|| type_err("WebAssembly.Table requires a descriptor"))?;
    let element = desc
        .get(js_string!("element"), ctx)?
        .to_string(ctx)?
        .to_std_string_escaped();
    let initial = to_u32(&desc.get(js_string!("initial"), ctx)?, ctx)?;
    let maximum = desc
        .get(js_string!("maximum"), ctx)
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u32);

    // Only funcref ("anyfunc") tables are supported in MVP.
    let elem_ty = if element == "anyfunc" || element == "funcref" {
        wasmi::ValType::FuncRef
    } else {
        return Err(type_err("WebAssembly.Table: unsupported element type"));
    };

    let mut store = wasm_store();
    let tty = wasmi::TableType::new(elem_ty, initial, maximum);
    let table = wasmi::Table::new(&mut store, tty, wasmi::Val::default(elem_ty))
        .map_err(|e| type_err(e.to_string()))?;

    let id = next_id();
    let len = table.size(&store);
    WASM_TABLES.with(|reg| {
        reg.borrow_mut()
            .insert(id, Rc::new(RefCell::new(WasmTable { store, table })))
    });

    let tobj = new_object(ctx);
    tobj.set(
        js_string!("__wasm_table"),
        JsValue::from(id as f64),
        true,
        ctx,
    )?;
    tobj.set(js_string!("length"), JsValue::from(len as f64), false, ctx)?;
    Ok(JsValue::from(tobj))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use boa_engine::{Context, JsValue, Source};

    /// Minimal valid WASM module: magic + version 1 + no sections.
    fn minimal_wasm() -> Vec<u8> {
        vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
    }

    fn js_bytes(bytes: &[u8]) -> String {
        let list = bytes
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!("new Uint8Array([{list}])")
    }

    // ── Task 2: validate + Module ───────────────────────────────────────────

    #[test]
    fn test_wasm_validate_true() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let js = format!("WebAssembly.validate({})", js_bytes(&minimal_wasm()));
        let result = ctx.eval(Source::from_bytes(&js)).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_wasm_validate_false() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let result = ctx
            .eval(Source::from_bytes(
                "WebAssembly.validate(new Uint8Array([0,0,0,0]))",
            ))
            .unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }

    #[test]
    fn test_wasm_module_compiles() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let js = format!(
            "typeof new WebAssembly.Module({}) === 'object'",
            js_bytes(&minimal_wasm())
        );
        let result = ctx.eval(Source::from_bytes(&js)).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    // ── Task 3: Instance + exported function calls ──────────────────────────

    #[test]
    fn test_wasm_instantiate_and_call() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        // (module (func (export "add") (param i32 i32) (result i32)
        //   local.get 0 local.get 1 i32.add))
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, // export "add"
            0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b, // code
        ];
        let js = format!(
            r#"
            var wasmBytes = {b};
            var module = new WebAssembly.Module(wasmBytes);
            var instance = new WebAssembly.Instance(module);
            instance.exports.add(2, 3)
            "#,
            b = js_bytes(wasm)
        );
        let result = ctx.eval(Source::from_bytes(&js)).unwrap();
        assert_eq!(result, JsValue::from(5));
    }

    // ── Task 4: host function imports ───────────────────────────────────────

    #[test]
    fn test_wasm_host_function_import() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        // (module
        //   (import "env" "double" (func $double (param i32) (result i32)))
        //   (func (export "call_double") (result i32) (call $double (i32.const 21))))
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f, // type (i32)->(i32)
            0x02, 0x0e, 0x01, 0x03, 0x65, 0x6e, 0x76, 0x06, 0x64, 0x6f, 0x75, 0x62, 0x6c, 0x65,
            0x00, 0x00, // import env.double
            0x03, 0x02, 0x01, 0x00, // function section
            0x07, 0x0f, 0x01, 0x0b, 0x63, 0x61, 0x6c, 0x6c, 0x5f, 0x64, 0x6f, 0x75, 0x62, 0x6c,
            0x65, 0x00, 0x01, // export call_double -> func 1
            0x0a, 0x08, 0x01, 0x06, 0x00, 0x41, 0x15, 0x10, 0x00, 0x0b, // code
        ];
        let js = format!(
            r#"
            var wasmBytes = {b};
            var module = new WebAssembly.Module(wasmBytes);
            var instance = new WebAssembly.Instance(module, {{
                env: {{ double: function(x) {{ return x * 2; }} }}
            }});
            instance.exports.call_double()
            "#,
            b = js_bytes(wasm)
        );
        let result = ctx.eval(Source::from_bytes(&js)).unwrap();
        assert_eq!(result, JsValue::from(42));
    }

    // ── Task 5: Memory ──────────────────────────────────────────────────────

    #[test]
    fn test_wasm_memory_grow_and_read() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let result = ctx
            .eval(Source::from_bytes(
                r#"
                var memory = new WebAssembly.Memory({ initial: 1 });
                memory.grow(1);
                memory.buffer.byteLength
                "#,
            ))
            .unwrap();
        // 2 pages × 65536 = 131072
        assert_eq!(result, JsValue::from(131_072));
    }

    // ── Task 6: Table + error types ─────────────────────────────────────────

    #[test]
    fn test_wasm_table() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let result = ctx
            .eval(Source::from_bytes(
                r#"
                var table = new WebAssembly.Table({ element: "anyfunc", initial: 2 });
                table.length
                "#,
            ))
            .unwrap();
        assert_eq!(result, JsValue::from(2));
    }

    #[test]
    fn test_wasm_compile_error() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let result = ctx
            .eval(Source::from_bytes(
                r#"
                try {
                    new WebAssembly.Module(new Uint8Array([0xFF, 0xFF]));
                    "no error";
                } catch (e) {
                    e instanceof WebAssembly.CompileError ? "compile error" : e.message;
                }
                "#,
            ))
            .unwrap();
        assert_eq!(result, JsValue::from(js_string!("compile error")));
    }

    #[test]
    fn test_wasm_compile_promise() {
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let js = format!(
            "WebAssembly.compile({b}) instanceof Promise",
            b = js_bytes(&minimal_wasm())
        );
        let result = ctx.eval(Source::from_bytes(&js)).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_wasm_fuel_exhaustion() {
        // An infinite loop must trap as a RuntimeError once the fuel budget
        // runs out, instead of hanging the JS thread — the same guarantee a
        // real browser provides. Bytes come from a WAT assembler (no hand
        // counting) via the `wat` dev-dependency.
        let wasm = wat::parse_str(r#"(module (func (export "spin") (loop (br 0))))"#)
            .expect("parse spin WAT");
        let bytes = wasm
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut ctx = Context::default();
        register_wasm_globals(&mut ctx).unwrap();
        let js = format!(
            r#"(function () {{
                try {{
                    var m = new WebAssembly.Module(new Uint8Array([{b}]));
                    var i = new WebAssembly.Instance(m);
                    i.exports.spin();
                    return "no-trap";
                }} catch (e) {{
                    return e instanceof WebAssembly.RuntimeError ? "RuntimeError" : ("other:" + (e && e.message));
                }}
            }})()"#,
            b = bytes
        );
        let result = ctx.eval(Source::from_bytes(&js)).unwrap();
        assert_eq!(result, JsValue::from(js_string!("RuntimeError")));
    }
}
