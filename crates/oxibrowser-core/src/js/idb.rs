//! IndexedDB (roadmap item 12 / FM-L5): the storage plane for IDB-auth
//! sites, so tokens survive envelope restore like localStorage.
//!
//! Deviations from the W3C spec, by design and documented:
//!
//! - **Synchronous execution, deferred events.** There is no disk I/O — the
//!   persistence plane is the session envelope — so operations apply to the
//!   in-memory state immediately and their `onsuccess` / `onupgradeneeded`
//!   events fire on the next JS pump (`drain_idb_events`, drained from
//!   `drain_timers` like WebSocket events). The canonical usage pattern —
//!   `var req = indexedDB.open(...); req.onsuccess = …` — works because
//!   handlers are assigned before the pump runs.
//! - **String keys + JSON values.** Keys are strings (numeric keys
//!   stringify); values are stored as their JSON serialization
//!   (`structured clone` of JSON-safe payloads — the token/record shape
//!   IDB-auth sites actually use).
//! - **Transactions are fat objects.** `transaction()` returns an object
//!   exposing `objectStore()`; mutations are durable immediately (the
//!   envelope capture is the durability boundary), and `oncomplete` fires
//!   after each mutating request rather than at a true commit point.
//! - No indexes, no cursors — extend when a measured site needs them.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use boa_engine::property::Attribute;
use boa_engine::{js_string, Context, JsError, JsObject, JsString, JsValue, JsNativeError, NativeFunction};
use serde_json::Value as Json;

use super::runtime::IndexedDbMsg;
use crate::storage_state::IdbDatabase;

/// One IndexedDB database's live state (version + object stores).
#[derive(Debug, Clone, Default)]
pub struct IdbDbState {
    pub version: u64,
    /// `store → key → record-json`.
    pub stores: BTreeMap<String, BTreeMap<String, String>>,
    /// Per-store keyPath (puts without an explicit key derive the key).
    pub key_paths: BTreeMap<String, Option<String>>,
}

type SharedDbs = Rc<RefCell<BTreeMap<String, IdbDbState>>>;
type SharedTx = Rc<RefCell<Option<std::sync::mpsc::Sender<IndexedDbMsg>>>>;

thread_local! {
    /// Deferred `(request, event)` pairs — fired on the next pump so the
    /// canonical "assign handler after the call returns" pattern works.
    static PENDING_IDB: RefCell<Vec<(JsObject, &'static str)>> = const { RefCell::new(Vec::new()) };
    /// The live database map for the current page origin (unused by the
    /// closures — they capture the per-registration `Rc` — but kept for
    /// future cross-frame introspection).
    static IDB_ORIGIN: RefCell<String> = RefCell::new(String::new());
}

/// Queue an event for the next pump.
fn queue_event(req: JsObject, event: &'static str) {
    PENDING_IDB.with(|q| q.borrow_mut().push((req, event)));
}

/// Drain pending IDB events: fire `on{event}` callbacks (handler set after
/// the call returned) with `{ target: request, type: event }`. Mirrors
/// `drain_ws_events`; called from `drain_timers`.
pub(crate) fn drain_idb_events(ctx: &mut Context) {
    let pending: Vec<(JsObject, &'static str)> =
        PENDING_IDB.with(|q| q.borrow_mut().drain(..).collect());
    for (req, event) in pending {
        let handler = req.get(js_string!(format!("on{event}").as_str()), ctx);
        if let Ok(cb) = handler
            && let Some(f) = cb.as_object()
            && f.is_callable()
        {
            let ev = JsObject::with_object_proto(ctx.intrinsics());
            let _ = ev.set(js_string!("target"), req.clone(), false, ctx);
            let _ = ev.set(js_string!("type"), js_string!(event), false, ctx);
            let _ = f.call(&req.clone().into(), &[ev.into()], ctx);
        }
    }
}

/// `DataError`-shaped JS error (IDB convention).
fn data_error(msg: impl std::fmt::Display) -> JsError {
    JsError::from_native(JsNativeError::error().with_message(format!("DataError: {msg}")))
}

fn json_of(value: &JsValue, ctx: &mut Context) -> Option<Json> {
    value.to_json(ctx).ok()
}

/// Build an `IDBRequest`-shaped object carrying `result`, queueing its
/// `success` event for the next pump.
fn make_request(ctx: &mut Context, result: JsValue) -> JsObject {
    let req = JsObject::with_object_proto(ctx.intrinsics());
    let _ = req.set(js_string!("result"), result, false, ctx);
    let _ = req.set(js_string!("onsuccess"), JsValue::null(), false, ctx);
    let _ = req.set(js_string!("onerror"), JsValue::null(), false, ctx);
    queue_event(req.clone(), "success");
    req
}

fn json_value(json: &Json, ctx: &mut Context) -> JsValue {
    JsValue::from_json(json, ctx).unwrap_or(JsValue::undefined())
}

/// Push the whole database blob to Session (full-blob sync — messages are
/// small, and last-wins is the same contract the envelope already has).
fn sync_db(tx: &SharedTx, origin: &str, name: &str, state: &IdbDbState) {
    if let Some(tx) = tx.borrow().as_ref() {
        let data = serde_json::json!({
            "stores": state.stores,
            "key_paths": state.key_paths,
        });
        let _ = tx.send(IndexedDbMsg::PutDb {
            origin: origin.to_string(),
            name: name.to_string(),
            version: state.version,
            data: data.to_string(),
        });
    }
}

/// Convert an `IdbDatabase` (envelope/serde shape) into live state.
fn live_state(db: &IdbDatabase) -> SharedDbs {
    Rc::new(RefCell::new(BTreeMap::from([(
        db.name.clone(),
        IdbDbState {
            version: db.version,
            stores: db.stores.clone(),
        key_paths: BTreeMap::new(),
    },
    )])))
}

/// Build a `store` object bound to `(db_name, store_name)`.
fn make_store(
    dbs: SharedDbs,
    db_name: String,
    store_name: String,
    key_path: Option<String>,
    tx: SharedTx,
    origin: String,
    on_complete: Option<JsObject>,
    ctx: &mut Context,
) -> JsObject {
    let obj = JsObject::with_object_proto(ctx.intrinsics());
    for (method, reject_existing) in [("put", false), ("add", true)] {
        let dbs = dbs.clone();
        let tx = tx.clone();
        let origin = origin.clone();
        let db_name = db_name.clone();
        let store_name = store_name.clone();
        let key_path = key_path.clone();
        let on_complete = on_complete.clone();
        let f = unsafe {
            NativeFunction::from_closure(move |_, args, ctx| {
                let value = args.first().cloned().unwrap_or(JsValue::undefined());
                let json: Json = json_of(&value, ctx)
                    .ok_or_else(|| data_error("value not serializable"))?;
                let explicit_key: Option<String> = match args.get(1) {
                    Some(k) if !k.is_undefined() => Some(
                        k.to_string(ctx)
                            .map_err(data_error)?
                            .to_std_string_escaped(),
                    ),
                    _ => None,
                };
                let key: String = match explicit_key {
                    Some(k) => k,
                    None => {
                        let Some(kp) = &key_path else {
                            return Err(data_error(format!(
                                "store uses out-of-line keys; pass a key (keyPath: {key_path:?})"
                            )));
                        };
                        match json.get(kp) {
                            Some(Json::Number(n)) => n.to_string(),
                            Some(Json::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => return Err(data_error("value missing keyPath field")),
                        }
                    }
                };
                let mut dbs = dbs.borrow_mut();
                let db = dbs.entry(db_name.clone()).or_default();
                let store = db.stores.entry(store_name.clone()).or_default();
                if reject_existing && store.contains_key(&key) {
                    return Err(data_error("key already exists"));
                }
                store.insert(key.clone(), serde_json::to_string(&json).unwrap_or_default());
                sync_db(&tx, &origin, &db_name, &db);
                // v1 transaction semantics: durable immediately; the parent
                // transaction completes after each mutating request. Request
                // success fires BEFORE the transaction complete (spec order).
                let req = JsObject::with_object_proto(ctx.intrinsics());
                let _ = req.set(js_string!("result"), JsValue::from(JsString::from(key.as_str())), false, ctx);
                queue_event(req.clone(), "success");
                if let Some(t) = &on_complete {
                    queue_event(t.clone(), "complete");
                }
                Ok(req.into())
            })
        };
        let _ = obj.set(
            js_string!(method),
            JsValue::from(f.to_js_function(ctx.realm())),
            false,
            ctx,
        );
    }

    // get(key)
    let get_dbs = dbs.clone();
    let get_db = db_name.clone();
    let get_store = store_name.clone();
    let get_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let key: String = match args.first() {
                Some(k) => k
                    .to_string(ctx)
                    .map_err(data_error)?
                    .to_std_string_escaped(),
                None => return Err(data_error("get requires a key")),
            };
            let value = get_dbs
                .borrow()
                .get(&get_db)
                .and_then(|db| db.stores.get(&get_store))
                .and_then(|store| store.get(&key))
                .and_then(|s| serde_json::from_str::<Json>(s).ok());
            let result = match value {
                Some(json) => json_value(&json, ctx),
                None => JsValue::undefined(),
            };
            Ok(make_request(ctx, result).into())
        })
    };
    let _ = obj.set(
        js_string!("get"),
        JsValue::from(get_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    // getAll() — every record's JSON value.
    let all_dbs = dbs.clone();
    let all_db = db_name.clone();
    let all_store = store_name.clone();
    let all_fn = unsafe {
        NativeFunction::from_closure(move |_, _args, ctx| {
            let records: Vec<Json> = all_dbs
                .borrow()
                .get(&all_db)
                .and_then(|db| db.stores.get(&all_store))
                .map(|store| {
                    store
                        .values()
                        .filter_map(|s| serde_json::from_str(s).ok())
                        .collect()
                })
                .unwrap_or_default();
            let mut arr = boa_engine::object::builtins::JsArray::new(ctx);
            for v in &records {
                arr.push(json_value(v, ctx), ctx)?;
            }
            Ok(arr.into())
        })
    };
    let _ = obj.set(
        js_string!("getAll"),
        JsValue::from(all_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    // delete(key)
    let del_dbs = dbs.clone();
    let del_tx = tx.clone();
    let del_origin = origin.clone();
    let del_db = db_name.clone();
    let del_store = store_name.clone();
    let del_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let key: String = match args.first() {
                Some(k) => k
                    .to_string(ctx)
                    .map_err(data_error)?
                    .to_std_string_escaped(),
                None => return Err(data_error("get requires a key")),
            };
            let mut dbs = del_dbs.borrow_mut();
            if let Some(db) = dbs.get_mut(&del_db)
                && let Some(store) = db.stores.get_mut(&del_store)
            {
                store.remove(&key);
                sync_db(&del_tx, &del_origin, &del_db, &db);
            }
            queue_event(make_request(ctx, JsValue::undefined()), "success");
            if let Some(t) = &on_complete {
                queue_event(t.clone(), "complete");
            }
            Ok(JsValue::undefined())
        })
    };
    let _ = obj.set(
        js_string!("delete"),
        JsValue::from(del_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    // count()
    let cnt_dbs = dbs.clone();
    let cnt_db = db_name.clone();
    let cnt_store = store_name.clone();
    let cnt_fn = unsafe {
        NativeFunction::from_closure(move |_, _args, ctx| {
            let n = cnt_dbs
                .borrow()
                .get(&cnt_db)
                .and_then(|db| db.stores.get(&cnt_store))
                .map(|s| s.len() as u32)
                .unwrap_or(0);
            Ok(make_request(ctx, JsValue::from(n)).into())
        })
    };
    let _ = obj.set(
        js_string!("count"),
        JsValue::from(cnt_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    obj
}

/// Build a `transaction` object exposing `objectStore(name)`.
fn make_transaction(
    dbs: SharedDbs,
    db_name: String,
    tx: SharedTx,
    origin: String,
    ctx: &mut Context,
) -> JsObject {
    let obj = JsObject::with_object_proto(ctx.intrinsics());
    let _ = obj.set(js_string!("mode"), js_string!("readwrite"), false, ctx);
    let obj_for_store = obj.clone();
    let os_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let name: String = match args.first() {
                    Some(n) => n.to_string(ctx).map_err(data_error)?.to_std_string_escaped(),
                    None => return Err(data_error("name requires a value")),
                };
            let exists = dbs
                .borrow()
                .get(&db_name)
                .is_some_and(|db| db.stores.contains_key(&name));
            if !exists {
                return Err(data_error(format!(
                    "object store \"{name}\" does not exist"
                )));
            }
            // The store's keyPath (recorded at createObjectStore) governs
            // keyless puts — the transaction path must carry it forward.
            let key_path = dbs
                .borrow()
                .get(&db_name)
                .and_then(|db| db.key_paths.get(&name).cloned())
                .unwrap_or(None);
            Ok(JsValue::from(make_store(
                dbs.clone(),
                db_name.clone(),
                name,
                key_path,
                tx.clone(),
                origin.clone(),
                Some(obj_for_store.clone()),
                ctx,
            )))
        })
    };
    let _ = obj.set(
        js_string!("objectStore"),
        JsValue::from(os_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );
    obj
}

/// Build the `IDBDatabase` object for `db_name`, wired to the live state.
fn make_database(
    dbs: SharedDbs,
    db_name: String,
    tx: SharedTx,
    origin: String,
    version: u64,
    ctx: &mut Context,
) -> JsObject {
    let obj = JsObject::with_object_proto(ctx.intrinsics());
    let _ = obj.set(js_string!("name"), js_string!(db_name.as_str()), false, ctx);
    let _ = obj.set(js_string!("version"), JsValue::from(version), false, ctx);

    let cos_dbs = dbs.clone();
    let cos_tx = tx.clone();
    let cos_origin = origin.clone();
    let cos_db = db_name.clone();
    let cos_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let name: String = match args.first() {
                    Some(n) => n.to_string(ctx).map_err(data_error)?.to_std_string_escaped(),
                    None => return Err(data_error("name requires a value")),
                };
            let key_path = args
                .get(1)
                .and_then(|o| o.as_object())
                .and_then(|o| o.get(js_string!("keyPath"), ctx).ok())
                .and_then(|k| k.as_string().map(|s| s.to_std_string_escaped()));
            // Mutate under the borrow, release it, then build the store
            // object (make_store re-borrows to bind its closures).
            let _version_now = {
                let mut m = cos_dbs.borrow_mut();
                let db = m.entry(cos_db.clone()).or_default();
                if db.stores.contains_key(&name) {
                    return Err(data_error(format!(
                        "store \"{name}\" already exists"
                    )));
                }
                db.stores.entry(name.clone()).or_default();
                // Record the keyPath (item 12): puts without an explicit key
                // derive theirs from this field; without it the transaction
                // path's store would be out-of-line and reject keyless puts.
                db.key_paths.insert(name.clone(), key_path.clone());
                db.version
            };
            Ok(JsValue::from(make_store(
                cos_dbs.clone(),
                cos_db.clone(),
                name,
                key_path,
                cos_tx.clone(),
                cos_origin.clone(),
                None,
                ctx,
            )))
        })
    };
    let _ = obj.set(
        js_string!("createObjectStore"),
        JsValue::from(cos_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    let del_dbs = dbs.clone();
    let del_db = db_name.clone();
    let del_del_tx = tx.clone();
    let del_del_origin = origin.clone();
    let del_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let name: String = match args.first() {
                    Some(n) => n.to_string(ctx).map_err(data_error)?.to_std_string_escaped(),
                    None => return Err(data_error("name requires a value")),
                };
            if let Some(db) = del_dbs.borrow_mut().get_mut(&del_db) {
                db.stores.remove(&name);
                db.key_paths.remove(&name);
                sync_db(&del_del_tx, &del_del_origin, &del_db, &db);
            }
            Ok(JsValue::undefined())
        })
    };
    let _ = obj.set(
        js_string!("deleteObjectStore"),
        JsValue::from(del_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    let tr_dbs = dbs.clone();
    let tr_tx = tx.clone();
    let tr_origin = origin.clone();
    let tr_db = db_name.clone();
    let tr_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let _ = args; // store names + mode accepted; not enforced (v1)
            Ok(JsValue::from(make_transaction(
                tr_dbs.clone(),
                tr_db.clone(),
                tr_tx.clone(),
                tr_origin.clone(),
                ctx,
            )))
        })
    };
    let _ = obj.set(
        js_string!("transaction"),
        JsValue::from(tr_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    let close_fn = unsafe {
        NativeFunction::from_closure(move |_, _args, _ctx| Ok(JsValue::undefined()))
    };
    let _ = obj.set(
        js_string!("close"),
        JsValue::from(close_fn.to_js_function(ctx.realm())),
        false,
        ctx,
    );

    obj
}

/// Register the `indexedDB` global over the live database map for `origin`.
///
/// `dbs` is the origin's seed (from the context bucket via `SetPageUrl`) and
/// is replaced wholesale on re-registration — the localStorage seeding
/// contract, applied to IDB.
pub fn register_indexed_db(
    ctx: &mut Context,
    dbs: BTreeMap<String, IdbDbState>,
    idb_tx: SharedTx,
    origin: String,
) {
    IDB_ORIGIN.with(|cell| *cell.borrow_mut() = origin.clone());

    let open_dbs: SharedDbs = Rc::new(RefCell::new(dbs));
    let open_tx = idb_tx.clone();
    let open_origin = origin.clone();
    let del_dbs: SharedDbs = open_dbs.clone();
    let del_tx = idb_tx.clone();
    let del_origin = open_origin.clone();
    let open_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let name: String = match args.first() {
                    Some(n) => n.to_string(ctx).map_err(data_error)?.to_std_string_escaped(),
                    None => return Err(data_error("name requires a value")),
                };
            let requested = args
                .get(1)
                .and_then(|v| v.as_number())
                .map(|v| v as u64)
                .unwrap_or(0);

            let req = JsObject::with_object_proto(ctx.intrinsics());
            let _ = req.set(js_string!("onupgradeneeded"), JsValue::null(), false, ctx);
            let _ = req.set(js_string!("onsuccess"), JsValue::null(), false, ctx);
            let _ = req.set(js_string!("onerror"), JsValue::null(), false, ctx);

            let current = open_dbs.borrow().get(&name).map(|d| d.version).unwrap_or(0);
            let version = if requested == 0 {
                current.max(1)
            } else {
                requested
            };
            if current > version {
                let _ = req.set(
                    js_string!("error"),
                    js_string!(format!(
                        "VersionError: database \"{name}\" exists at version {current} (requested {version})"
                    )),
                    false,
                    ctx,
                );
                queue_event(req.clone(), "error");
                return Ok(req.into());
            }

            // Version bump applies synchronously — the envelope capture is
            // the durability plane.
            {
                let mut dbs = open_dbs.borrow_mut();
                let db = dbs.entry(name.clone()).or_default();
                db.version = version;
                sync_db(&open_tx, &open_origin, &name, &db);
            }

            // Expose the database handle before events fire so
            // `onupgradeneeded` handlers can createObjectStore on it.
            let db_obj = make_database(
                open_dbs.clone(),
                name.clone(),
                open_tx.clone(),
                open_origin.clone(),
                version,
                ctx,
            );
            let _ = req.set(js_string!("result"), db_obj.clone(), false, ctx);
            if current < version {
                queue_event(req.clone(), "upgradeneeded");
            }
            queue_event(req.clone(), "success");
            Ok(req.into())
        })
    };

    let delete_fn = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let name: String = match args.first() {
                    Some(n) => n.to_string(ctx).map_err(data_error)?.to_std_string_escaped(),
                    None => return Err(data_error("name requires a value")),
                };
            del_dbs.borrow_mut().remove(&name);
            if let Some(tx) = del_tx.borrow().as_ref() {
                let _ = tx.send(IndexedDbMsg::DeleteDb {
                    origin: del_origin.clone(),
                    name: name.clone(),
                });
            }
            let req = JsObject::with_object_proto(ctx.intrinsics());
            let _ = req.set(js_string!("onsuccess"), JsValue::null(), false, ctx);
            queue_event(req.clone(), "success");
            Ok(req.into())
        })
    };

    let idb_obj = boa_engine::object::ObjectInitializer::new(ctx)
        .function(open_fn, js_string!("open"), 1)
        .function(delete_fn, js_string!("deleteDatabase"), 1)
        .build();
    let _ = ctx.register_global_property(js_string!("indexedDB"), idb_obj, Attribute::all());
}

/// Seed a single seeded database (used by `register_indexed_db` callers that
/// already hold the per-origin map) — conversion helper for the serde shape.
#[allow(dead_code)]
fn live_from_envelope(db: &IdbDatabase) -> SharedDbs {
    Rc::new(RefCell::new(BTreeMap::from([(
        db.name.clone(),
        IdbDbState {
            version: db.version,
            stores: db.stores.clone(),
        key_paths: BTreeMap::new(),
    },
    )])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_engine::Source;

    fn eval(src: &str, ctx: &mut Context) -> JsValue {
        ctx.eval(Source::from_bytes(src)).unwrap()
    }

    #[test]
    fn keypath_put_get_roundtrip() {
        let mut ctx = Context::default();
        let (tx, rx) = std::sync::mpsc::channel::<IndexedDbMsg>();
        let tx_cell: SharedTx = Rc::new(RefCell::new(Some(tx)));
        register_indexed_db(
            &mut ctx,
            BTreeMap::new(),
            tx_cell.clone(),
            "https://example.com".into(),
        );

        eval(
            r#"(function(){
                var req = indexedDB.open("auth", 1);
                __idb_flags = { upgraded: false, done: false, ok: false };
                req.onupgradeneeded = function () {
                    __idb_flags.upgraded = true;
                    req.result.createObjectStore("tokens", { keyPath: "id" });
                };
                req.onsuccess = function () { __idb_flags.done = true; __idb_flags.ok = true; };
                req.onerror = function () { __idb_flags.done = true; };
                __idb_test = { req: req };
            })()"#,
            &mut ctx,
        );

        // Pump until the deferred events fire.
        for _ in 0..5 {
            drain_idb_events(&mut ctx);
        }

        let upgraded = eval("__idb_flags.upgraded", &mut ctx);
        assert_eq!(upgraded.as_boolean(), Some(true), "upgrade must fire");
        let done = eval("__idb_flags.done", &mut ctx);
        assert_eq!(done.as_boolean(), Some(true));
        let ok = eval("__idb_flags.ok", &mut ctx);
        assert_eq!(ok.as_boolean(), Some(true));

        eval(
            r#"(function(){
                const tx = __idb_test.req.result.transaction("tokens", "readwrite");
                const put = tx.objectStore("tokens").put({ id: "gcp", token: "tok-123" });
                put.onsuccess = () => { __idb_flags.put_key = put.result; };
            })()"#,
            &mut ctx,
        );
        for _ in 0..5 {
            drain_idb_events(&mut ctx);
        }
        let put_key = eval("__idb_flags.put_key", &mut ctx);
        assert_eq!(
            put_key.as_string().map(|s| s.to_std_string_escaped()),
            Some("gcp".to_string())
        );

        eval(
            r#"(function(){
                const tx = __idb_test.req.result.transaction("tokens", "readonly");
                const get = tx.objectStore("tokens").get("gcp");
                get.onsuccess = () => { __idb_flags.token = get.result ? get.result.token : "MISSING"; };
            })()"#,
            &mut ctx,
        );
        for _ in 0..5 {
            drain_idb_events(&mut ctx);
        }
        let token = eval("__idb_flags.token", &mut ctx);
        assert_eq!(
            token.as_string().map(|s| s.to_std_string_escaped()),
            Some("tok-123".to_string())
        );

        // Full-blob sync reached the channel: the open-time version bump
        // emits an empty blob first; the LAST PutDb carries the record.
        let mut put_data = String::new();
        for msg in rx.try_iter() {
            match msg {
                IndexedDbMsg::PutDb { origin, name, data, .. } => {
                    assert_eq!(origin, "https://example.com");
                    assert_eq!(name, "auth");
                    put_data = data;
                }
                _ => panic!("expected PutDb"),
            }
        }
        assert!(
            put_data.contains("tok-123") && put_data.contains("\"key_paths\""),
            "{put_data}"
        );
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;
    use boa_engine::Source;

    /// Mirror of the Session flow: register → open/put (JS) → apply PutDb
    /// messages to the context bucket → re-register from the bucket → get.
    #[test]
    fn token_survives_reregistration() {
        let mut ctx = Context::default();
        let (tx, rx) = std::sync::mpsc::channel::<IndexedDbMsg>();
        let tx_cell: SharedTx = Rc::new(RefCell::new(Some(tx)));
        register_indexed_db(
            &mut ctx,
            BTreeMap::new(),
            tx_cell.clone(),
            "https://shop.io".into(),
        );
        ctx.eval(Source::from_bytes("var __idb_put_ok = false;")).unwrap();

        let put = r#"(function(){
            const req = indexedDB.open("auth", 1);
            req.onupgradeneeded = () => req.result.createObjectStore("tokens", { keyPath: "id" });
            req.onsuccess = () => {
                const tx = req.result.transaction("tokens", "readwrite");
                tx.objectStore("tokens").put({ id: "gcp", token: "tok-123" });
                tx.oncomplete = () => { __idb_put_ok = true; };
            };
        })()"#;
        ctx.eval(Source::from_bytes(put)).unwrap();
        // Deferred IDB events fire on the pump (the Session side drains via
        // `drain_timers` — here we pump explicitly).
        for _ in 0..5 {
            drain_idb_events(&mut ctx);
        }
        let put_flag = ctx
            .eval(Source::from_bytes("__idb_put_ok"))
            .unwrap()
            .as_boolean();
        assert_eq!(put_flag, Some(true), "put must succeed");

        // Apply sync messages to the "context bucket".
        let mut bucket: BTreeMap<String, IdbDbState> = BTreeMap::new();
        for msg in rx.try_iter() {
            match msg {
                IndexedDbMsg::PutDb { origin, name, version, data } => {
                    assert_eq!(origin, "https://shop.io");
                    let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
                    let stores = serde_json::from_value(parsed["stores"].clone()).unwrap();
                    let key_paths = serde_json::from_value(parsed["key_paths"].clone()).unwrap();
                    bucket.insert(name, IdbDbState { version, stores, key_paths });
                }
                _ => {}
            }
        }
        let auth = bucket.get("auth").expect("PutDb must carry the auth db");
        assert_eq!(auth.key_paths.get("tokens").cloned(), Some(Some("id".to_string())));
        assert!(auth.stores["tokens"]["gcp"].contains("tok-123"));

        // Re-registration from the bucket (the navigation seed) — fresh map.
        let seed: BTreeMap<String, IdbDbState> = bucket
            .iter()
            .map(|(n, d)| (n.clone(), d.clone()))
            .collect();
        register_indexed_db(&mut ctx, seed, tx_cell, "https://shop.io".into());

        ctx.eval(Source::from_bytes(
            "(function(){ __idb_get = { state: 'open' };              const req = indexedDB.open('auth');              req.onsuccess = () => { __idb_get.state = 'open-ok';                const tx = req.result.transaction('tokens', 'readonly');                const get = tx.objectStore('tokens').get('gcp');                get.onsuccess = () => { __idb_get.state = String(get.result ? get.result.token : 'MISSING'); }; }; })()",
        ))
        .unwrap();
        for _ in 0..5 {
            drain_idb_events(&mut ctx);
        }
        let got = ctx
            .eval(Source::from_bytes("__idb_get.state"))
            .unwrap()
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        assert_eq!(got, "tok-123", "token must survive re-registration");
    }
}
