//! Buffered XHR over the shared HTTP bridge, including genuine binary responses.
//! Event/state references stay in JS objects; native closures capture no GC values.

use super::*;
use boa_engine::object::builtins::{AlignedVec, JsArrayBuffer};

pub(super) fn mirror_window(ctx: &mut Context) -> JsResult<()> {
    let constructor = ctx.global_object().get(js_string!("XMLHttpRequest"), ctx)?;
    if let Some(window) = ctx
        .global_object()
        .get(js_string!("window"), ctx)?
        .as_object()
    {
        window.set(js_string!("XMLHttpRequest"), constructor, true, ctx)?;
    }
    Ok(())
}

pub(super) fn register(
    ctx: &mut Context,
    fetch_tx: Arc<RwLock<Option<Sender<FetchRequestMsg>>>>,
) -> JsResult<()> {
    // SAFETY: the only capture is a channel holder containing no Boa GC values.
    let send = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let xhr = args.first().and_then(JsValue::as_object).ok_or_else(|| {
                JsNativeError::typ().with_message("Invalid XMLHttpRequest receiver")
            })?;
            let state = args
                .get(1)
                .and_then(JsValue::as_object)
                .ok_or_else(|| JsNativeError::typ().with_message("Invalid XMLHttpRequest state"))?;
            let get_string = |key: &str, ctx: &mut Context| -> JsResult<String> {
                state
                    .get(JsString::from(key), ctx)?
                    .to_string(ctx)
                    .map(|value| value.to_std_string_lossy())
            };
            let method = get_string("method", ctx)?;
            let url = get_string("url", ctx)?;
            let is_async = state.get(js_string!("async"), ctx)?.to_boolean();
            let header_list = state
                .get(js_string!("requestHeaders"), ctx)?
                .as_object()
                .ok_or_else(|| JsNativeError::typ().with_message("Invalid headers"))?;
            let length = header_list.get(js_string!("length"), ctx)?.to_length(ctx)?;
            let mut headers = Vec::new();
            for index in 0..length {
                let pair = header_list
                    .get(index, ctx)?
                    .as_object()
                    .ok_or_else(|| JsNativeError::typ().with_message("Invalid header"))?;
                headers.push((
                    pair.get(0, ctx)?.to_string(ctx)?.to_std_string_lossy(),
                    pair.get(1, ctx)?.to_string(ctx)?.to_std_string_lossy(),
                ));
            }
            let (body, content_type) = args
                .get(2)
                .and_then(|value| normalize_fetch_body(value, ctx))
                .map_or((None, None), |(bytes, content_type)| {
                    (Some(bytes), content_type)
                });
            if let Some(content_type) = content_type
                && !headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("content-type"))
            {
                headers.push(("content-type".into(), content_type));
            }
            let id = next_fetch_id();
            state.set(js_string!("id"), id as f64, true, ctx)?;
            PENDING_FETCH.with(|pending| {
                pending.borrow_mut().insert(
                    id,
                    PendingFetch::Xhr {
                        object: xhr.clone(),
                        state: state.clone(),
                        context_id: active_context_id(),
                    },
                );
            });
            if event_sink_attached() {
                push_event(CoreEvent::FetchRequest {
                    request_id: cdp_request_id(id),
                    url: url.clone(),
                    method: method.clone(),
                    headers: headers.clone(),
                    post_data: body.clone(),
                    timestamp: now_ms(),
                });
            }
            let request = FetchRequestMsg {
                id,
                url: url.clone(),
                method,
                headers,
                body,
                origin: current_origin(),
            };
            let channel = fetch_tx.read().clone();
            let sent = channel.is_some_and(|channel| channel.send(request).is_ok());
            if !sent || (!is_async && !wait_for_sync_xhr(id, ctx)) {
                PENDING_FETCH.with(|pending| {
                    pending.borrow_mut().remove(&id);
                });
                settle(
                    xhr,
                    state,
                    FetchResponseMsg {
                        id,
                        status: 0,
                        status_text: String::new(),
                        url,
                        headers: Vec::new(),
                        body: String::new(),
                        body_bytes: None,
                        error: Some("XHR transport unavailable".into()),
                    },
                    ctx,
                );
            }
            Ok(JsValue::undefined())
        })
    };
    ctx.register_global_callable(js_string!("__oxi_xhr_send"), 3, send)?;
    ctx.register_global_callable(
        js_string!("__oxi_xhr_abort"),
        1,
        NativeFunction::from_fn_ptr(|_, args, _| {
            if let Some(id) = args.first().and_then(JsValue::as_number) {
                PENDING_FETCH.with(|pending| {
                    pending.borrow_mut().remove(&(id as u64));
                });
            }
            Ok(JsValue::undefined())
        }),
    )?;
    ctx.eval(Source::from_bytes(BOOTSTRAP))?;
    Ok(())
}

pub(super) fn settle(object: JsObject, state: JsObject, resp: FetchResponseMsg, ctx: &mut Context) {
    let result = (|| -> JsResult<()> {
        let error = resp.error.is_some();
        let bytes = resp
            .body_bytes
            .unwrap_or_else(|| resp.body.as_bytes().to_vec());
        let loaded = bytes.len();
        // The response type may still change in HEADERS_RECEIVED callbacks.
        // Keep the genuine buffer ready before those callbacks can run.
        let buffer = if !error {
            JsArrayBuffer::from_byte_block(AlignedVec::from_iter(0, bytes), ctx)?.into()
        } else {
            JsValue::null()
        };
        let mut headers = String::new();
        for (key, value) in &resp.headers {
            if !key.eq_ignore_ascii_case("set-cookie") && !key.eq_ignore_ascii_case("set-cookie2") {
                headers.push_str(key);
                headers.push_str(": ");
                headers.push_str(value);
                headers.push_str("\r\n");
            }
        }
        let finish = state
            .get(js_string!("finish"), ctx)?
            .as_callable()
            .ok_or_else(|| JsNativeError::typ().with_message("Invalid XHR completion callback"))?;
        finish.call(
            &object.into(),
            &[
                JsValue::from(resp.id as f64),
                JsValue::from(resp.status),
                JsString::from(resp.status_text.as_str()).into(),
                JsString::from(resp.url.as_str()).into(),
                JsString::from(headers.as_str()).into(),
                JsString::from(resp.body.as_str()).into(),
                buffer,
                JsValue::from(loaded as f64),
                JsValue::from(error),
            ],
            ctx,
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "XHR completion failed");
    }
}

const BOOTSTRAP: &str = r#"
(function () {
  const send = globalThis.__oxi_xhr_send, cancel = globalThis.__oxi_xhr_abort;
  delete globalThis.__oxi_xhr_send; delete globalThis.__oxi_xhr_abort;
  const states = new WeakMap();
  function fail(message, name = 'InvalidStateError') { const error = new Error(message); error.name = name; return error; }
  function state(xhr) { const value = states.get(xhr); if (!value) throw new TypeError('Invalid XMLHttpRequest receiver'); return value; }
  function dispatch(xhr, type, loaded = 0, total = 0) {
    const s = state(xhr);
    const event = {type, target: xhr, currentTarget: xhr, bubbles: false, cancelable: false,
      defaultPrevented: false, lengthComputable: total > 0, loaded, total,
      timeStamp: typeof performance === 'object' ? performance.now() : 0};
    function invoke(callback) {
      try {
        if (typeof callback === 'function') callback.call(xhr, event);
        else if (callback && typeof callback.handleEvent === 'function') callback.handleEvent(event);
      } catch (error) { console.error(error); }
    }
    const property = xhr['on' + type];
    const listeners = (s.listeners.get(type) || []).slice();
    if (typeof property === 'function') invoke(property);
    for (const entry of listeners) {
      if (!(s.listeners.get(type) || []).includes(entry)) continue;
      if (entry.once) xhr.removeEventListener(type, entry.callback, entry.capture);
      invoke(entry.callback);
    }
    event.currentTarget = null;
  }
  function clearResponse(s) {
    s.status = 0; s.statusText = ''; s.responseURL = ''; s.responseText = '';
    s.responseHeaders = ''; s.binary = null;
  }
  class XMLHttpRequest {
    constructor() {
      const xhr = this;
      const s = {readyState: 0, responseType: '', async: true, sent: false, id: null,
        listeners: new Map(), requestHeaders: [], method: 'GET', url: '', sequence: 0};
      clearResponse(s); states.set(xhr, s);
      for (const name of ['readystatechange','loadstart','progress','load','error','abort','loadend','timeout']) this['on'+name] = null;
      this.timeout = 0; this.withCredentials = false;
      s.finish = function(id, status, statusText, url, headers, body, binary, loaded, error) {
        const current = () => s.sent && s.id === id;
        if (!current()) return;
        if (error) clearResponse(s);
        else {
          s.status = status; s.statusText = statusText; s.responseURL = url; s.responseHeaders = headers;
          s.readyState = 2; dispatch(xhr, 'readystatechange'); if (!current()) return;
          s.responseText = body; s.binary = binary;
          s.readyState = 3; dispatch(xhr, 'readystatechange'); if (!current()) return;
          dispatch(xhr, 'progress', loaded, loaded); if (!current()) return;
        }
        s.readyState = 4; s.sent = false;
        dispatch(xhr, 'readystatechange'); if (s.id !== id) return;
        dispatch(xhr, error ? 'error' : 'load', error ? 0 : loaded, error ? 0 : loaded);
        if (s.id !== id) return;
        dispatch(xhr, 'loadend', error ? 0 : loaded, error ? 0 : loaded);
      };
    }
    get readyState() { return state(this).readyState; }
    get status() { return state(this).status; }
    get statusText() { return state(this).statusText; }
    get responseURL() { return state(this).responseURL; }
    get responseType() { return state(this).responseType; }
    set responseType(value) {
      const s = state(this); value = String(value);
      if (s.readyState === 3 || s.readyState === 4) throw fail('Response is already loading or done');
      if (!['', 'text', 'arraybuffer'].includes(value)) throw fail('Unsupported XHR response type', 'NotSupportedError');
      s.responseType = value;
    }
    get responseText() {
      const s = state(this);
      if (s.responseType !== '' && s.responseType !== 'text') throw fail('Response is not text');
      return s.readyState < 3 ? '' : s.responseText;
    }
    get response() {
      const s = state(this);
      return s.responseType === 'arraybuffer' ? (s.readyState === 4 ? s.binary : null) : (s.readyState < 3 ? '' : s.responseText);
    }
    open(method, url, async = true) {
      const s = state(this);
      method = String(method).toUpperCase();
      if (!/^[!#$%&'*+.^_`|~0-9A-Z-]+$/.test(method)) throw new TypeError('Invalid HTTP method');
      if (['CONNECT','TRACE','TRACK'].includes(method)) throw fail('Forbidden HTTP method', 'SecurityError');
      const base = globalThis.location && globalThis.location.href;
      const absolute = base ? new URL(String(url), base).href : new URL(String(url)).href;
      cancel(s.id); s.id = null; s.sent = false; s.sequence++; clearResponse(s);
      s.method = method; s.url = absolute; s.async = Boolean(async); s.requestHeaders = [];
      s.readyState = 1; dispatch(this, 'readystatechange');
    }
    send(body = null) {
      const s = state(this);
      if (s.readyState !== 1 || s.sent) throw fail('Request is not opened or already sent');
      if (Number(this.timeout) !== 0) throw fail('XHR timeouts are unsupported', 'NotSupportedError');
      const sequence = s.sequence;
      s.sent = true; dispatch(this, 'loadstart');
      if (s.sent && s.sequence === sequence) send(this, s, ['GET','HEAD'].includes(s.method) ? null : body);
    }
    abort() {
      const s = state(this), active = s.sent;
      cancel(s.id); s.id = null; s.sent = false; const sequence = ++s.sequence; clearResponse(s);
      if (active) {
        s.readyState = 4; dispatch(this, 'readystatechange'); if (s.sequence !== sequence) return;
        dispatch(this, 'abort'); if (s.sequence !== sequence) return;
        dispatch(this, 'loadend');
      }
      if (s.sequence === sequence) s.readyState = 0;
    }
    setRequestHeader(name, value) {
      const s = state(this);
      if (s.readyState !== 1 || s.sent) throw fail('Request is not opened or already sent');
      name = String(name).toLowerCase(); value = String(value).trim();
      if (!/^[!#$%&'*+.^_`|~0-9a-z-]+$/.test(name) || /[\r\n\0]/.test(value)) throw new TypeError('Invalid request header');
      if (['accept-charset','accept-encoding','access-control-request-headers','access-control-request-method',
           'connection','content-length','cookie','cookie2','date','dnt','expect','host','keep-alive','origin',
           'referer','te','trailer','transfer-encoding','upgrade','via'].includes(name) || /^(proxy-|sec-)/.test(name)) return;
      const entry = s.requestHeaders.find(entry => entry[0] === name);
      if (entry) entry[1] += ', ' + value; else s.requestHeaders.push([name, value]);
    }
    getResponseHeader(name) {
      const s = state(this); name = String(name).toLowerCase();
      if (s.readyState < 2 || name === 'set-cookie' || name === 'set-cookie2') return null;
      const values = [];
      for (const line of s.responseHeaders.split('\r\n')) {
        const colon = line.indexOf(':');
        if (colon >= 0 && line.slice(0, colon).trim().toLowerCase() === name) values.push(line.slice(colon + 1).trim());
      }
      return values.length ? values.join(', ') : null;
    }
    getAllResponseHeaders() { const s = state(this); return s.readyState >= 2 ? s.responseHeaders : ''; }
    overrideMimeType(value) {
      const s = state(this);
      if (s.readyState >= 3) throw fail('Response is already loading or done');
      const parts = String(value).split(';');
      if (!parts[0].includes('/')) throw new TypeError('overrideMimeType expects a MIME type');
      for (const part of parts.slice(1)) {
        const match = /^\s*charset\s*=\s*["']?([^"'\s;]+)/i.exec(part);
        if (match && !['utf-8','utf8','us-ascii'].includes(match[1].toLowerCase()))
          throw new TypeError('only UTF-8 XHR response decoding is supported');
      }
    }
    addEventListener(type, callback, options = {}) {
      const s = state(this); if (callback === null || callback === undefined) return;
      type = String(type); const capture = typeof options === 'boolean' ? options : !!(options && options.capture);
      const listeners = s.listeners.get(type) || [];
      if (!listeners.some(entry => entry.callback === callback && entry.capture === capture))
        listeners.push({callback, capture, once: !!(options && options.once)});
      s.listeners.set(type, listeners);
    }
    removeEventListener(type, callback, options = {}) {
      const s = state(this); type = String(type);
      const capture = typeof options === 'boolean' ? options : !!(options && options.capture);
      s.listeners.set(type, (s.listeners.get(type) || []).filter(entry => entry.callback !== callback || entry.capture !== capture));
    }
  }
  for (const [key, value] of Object.entries({UNSENT:0, OPENED:1, HEADERS_RECEIVED:2, LOADING:3, DONE:4})) {
    Object.defineProperty(XMLHttpRequest, key, {value, enumerable:true});
    Object.defineProperty(XMLHttpRequest.prototype, key, {value, enumerable:true});
  }
  Object.defineProperty(XMLHttpRequest.prototype, Symbol.toStringTag, {value:'XMLHttpRequest', configurable:true});
  globalThis.XMLHttpRequest = XMLHttpRequest;
  if (globalThis.window) globalThis.window.XMLHttpRequest = XMLHttpRequest;
})();
"#;
