//! Real, bounded raster image loading for Image/HTML img elements.
//!
//! Data and branded Blob URLs decode locally. Same-origin HTTP uses the
//! existing fetch bridge; cross-origin images and redirects are unsupported.
//! Pixel state lives in JS WeakMaps, never untraced native closures.

use super::*;
use boa_engine::object::builtins::{AlignedVec, JsArrayBuffer};

const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_DIMENSION: u32 = 16_384;

fn image_error(message: impl Into<String>) -> boa_engine::JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

fn byte_buffer(bytes: Vec<u8>, ctx: &mut Context) -> JsResult<JsValue> {
    JsArrayBuffer::from_byte_block(AlignedVec::from_iter(0, bytes), ctx).map(Into::into)
}

fn data_bytes(source: &str) -> JsResult<Vec<u8>> {
    if source.len() > MAX_BYTES {
        return Err(image_error("Image source exceeds 128 MiB limit"));
    }
    let mut url = url::Url::parse(source).map_err(|_| image_error("Invalid image data URL"))?;
    if url.scheme() != "data" {
        return Err(image_error("Expected an image data URL"));
    }
    url.set_fragment(None);
    let (metadata, body) = url.as_str()[5..]
        .split_once(',')
        .ok_or_else(|| image_error("Invalid image data URL"))?;
    let body: Vec<u8> = percent_encoding::percent_decode_str(body).collect();
    if metadata
        .split(';')
        .next_back()
        .is_some_and(|part| part.trim().eq_ignore_ascii_case("base64"))
    {
        let body: Vec<u8> = body
            .into_iter()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        let config = base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
        base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD, config)
            .decode(body)
            .map_err(|_| image_error("Invalid base64 image data"))
    } else {
        Ok(body)
    }
}

fn decode(bytes: &[u8]) -> JsResult<(u32, u32, Vec<u8>)> {
    use ::image::ImageDecoder;
    if bytes.len() > MAX_BYTES {
        return Err(image_error("Encoded image exceeds 128 MiB limit"));
    }
    let mut reader = ::image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| image_error(format!("Image format detection failed: {error}")))?;
    let mut limits = ::image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_BYTES as u64);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| image_error(format!("Image decode failed: {error}")))?;
    let (width, height) = decoder.dimensions();
    let rgba_len = (width as u64)
        .checked_mul(height as u64)
        .and_then(|pixels| pixels.checked_mul(4));
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || rgba_len.is_none_or(|length| length > MAX_BYTES as u64)
        || decoder.total_bytes() > MAX_BYTES as u64
    {
        return Err(image_error(
            "Decoded image exceeds 16k dimensions or 128 MiB limit",
        ));
    }
    let pixels = ::image::DynamicImage::from_decoder(decoder)
        .map_err(|error| image_error(format!("Image decode failed: {error}")))?
        .into_rgba8()
        .into_raw();
    Ok((width, height, pixels))
}

pub(super) fn register(ctx: &mut Context) -> JsResult<()> {
    ctx.register_global_callable(
        js_string!("__oxi_image_decode"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let bytes = binary_streams::bytes(args.first().unwrap_or(&JsValue::undefined()), ctx)?;
            let (width, height, pixels) = decode(&bytes)?;
            let pixels = byte_buffer(pixels, ctx)?;
            Ok(boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("width"), width, Attribute::all())
                .property(js_string!("height"), height, Attribute::all())
                .property(js_string!("data"), pixels, Attribute::all())
                .build()
                .into())
        }),
    )?;
    ctx.register_global_callable(
        js_string!("__oxi_image_data"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let source = args
                .first()
                .unwrap_or(&JsValue::undefined())
                .to_string(ctx)?
                .to_std_string_lossy();
            byte_buffer(data_bytes(&source)?, ctx)
        }),
    )?;
    ctx.register_global_callable(
        js_string!("__oxi_image_url"),
        0,
        NativeFunction::from_fn_ptr(|_, _, _| {
            let origin = current_origin().unwrap_or_else(|| "null".into());
            Ok(JsString::from(format!("blob:{origin}/{}", uuid::Uuid::new_v4()).as_str()).into())
        }),
    )?;
    ctx.eval(Source::from_bytes(BOOTSTRAP))?;
    Ok(())
}

pub(super) fn mirror_window(ctx: &mut Context) {
    let _ = ctx.eval(Source::from_bytes("if (globalThis.__oxiAttachImage && globalThis.window) { window.Image = globalThis.Image; window.HTMLImageElement = globalThis.HTMLImageElement; __oxiInstallImageLifecycle(); }"));
}

pub(super) fn attach(ctx: &mut Context, object: &JsObject, node_id: usize) {
    let helper = ctx.global_object().get(js_string!("__oxiAttachImage"), ctx);
    if let Ok(helper) = helper
        && let Some(helper) = helper.as_callable()
        && let Err(error) = helper.call(
            &JsValue::undefined(),
            &[object.clone().into(), JsValue::from(node_id as f64)],
            ctx,
        )
    {
        tracing::warn!(%error, "Image element hooks failed");
    }
}

pub(super) fn reset(ctx: &mut Context) {
    let _ = ctx.eval(Source::from_bytes(
        "if (globalThis.__oxiResetImages) __oxiResetImages();",
    ));
}

/// Native subtree replacement bypasses the per-node disconnect hooks. Invoke
/// only after releasing the renderer borrow: the sweep reads live DOM parents.
pub(super) fn sweep_connected(ctx: &mut Context) {
    let helper = ctx.global_object().get(js_string!("__oxiSweepImages"), ctx);
    if let Ok(helper) = helper
        && let Some(helper) = helper.as_callable()
        && let Err(error) = helper.call(&JsValue::undefined(), &[], ctx)
    {
        tracing::warn!(%error, "Disconnected image state sweep failed");
    }
}

/// Genuine decoded Image sources for WebGL uploads. Other source kinds return
/// None; branded images that are empty, loading or broken throw InvalidStateError.
#[cfg_attr(not(feature = "webgl"), allow(dead_code))]
pub(crate) fn source_pixels(
    source: &JsValue,
    ctx: &mut Context,
) -> JsResult<Option<(u32, u32, Vec<u8>)>> {
    let helper = ctx
        .global_object()
        .get(js_string!("__oxiImagePixels"), ctx)?;
    let Some(helper) = helper.as_callable() else {
        return Ok(None);
    };
    let result = helper.call(&JsValue::undefined(), std::slice::from_ref(source), ctx)?;
    let Some(object) = result.as_object() else {
        return Ok(None);
    };
    let width = object.get(js_string!("width"), ctx)?.to_u32(ctx)?;
    let height = object.get(js_string!("height"), ctx)?.to_u32(ctx)?;
    let data = object.get(js_string!("data"), ctx)?;
    let pixels = copy_buffer_source(&data, ctx)?;
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || pixels.len() > MAX_BYTES
        || u64::from(width) * u64::from(height) * 4 != pixels.len() as u64
    {
        return Err(image_error("Invalid decoded image pixel storage"));
    }
    Ok(Some((width, height, pixels)))
}

const BOOTSTRAP: &str = r#"
(function () {
  const decodePixels = globalThis.__oxi_image_decode, dataBytes = globalThis.__oxi_image_data;
  const mintURL = globalThis.__oxi_image_url, blobBytes = globalThis.__oxiBlobBytes;
  delete globalThis.__oxi_image_decode; delete globalThis.__oxi_image_data;
  delete globalThis.__oxi_image_url; delete globalThis.__oxiBlobBytes;
  const URLCtor = globalThis.URL, fetchImage = globalThis.fetch;
  const images = new WeakMap(), byNode = new Map(), urls = new Map(), connected = new Map();
  const LIMIT = 128 * 1024 * 1024;
  let pending = new Set(), epoch = 1, urlBytes = 0;
  const wrappedLifecycle = new Map();
  function isConnected(image) {
    const documentRoot = document.documentElement;
    if (!documentRoot) return false;
    const seen = new Set(); let root = image;
    while (root) {
      if (root.__nodeId === documentRoot.__nodeId) return true;
      if (seen.has(root.__nodeId)) return false;
      seen.add(root.__nodeId); root = root.parentNode;
    }
    return false;
  }
  function track(state) {
    if (typeof state.node !== 'number') return;
    if (state.epoch === epoch && isConnected(state.owner)) connected.set(state.node, state);
    else connected.delete(state.node);
  }
  function installLifecycle() {
    for (const kind of ['connected', 'disconnected']) {
      const key = '__oxi_fire_' + kind, base = globalThis[key];
      if (typeof base !== 'function' || base === wrappedLifecycle.get(key)) continue;
      const wrapper = function(node) {
        base.call(this, node);
        const stack = [node];
        while (stack.length) {
          const element = stack.pop(); if (!element) continue;
          const state = images.get(element) || (byNode.get(element.__nodeId) || {}).deref?.();
          if (state) track(state);
          const children = element.children || element.childNodes;
          if (children) for (let i = 0; i < children.length; i++) stack.push(children[i]);
        }
      };
      wrappedLifecycle.set(key, wrapper); globalThis[key] = wrapper;
    }
  }
  function failure(name, message) { const error = new Error(message); error.name = name; return error; }
  function encoding(message) { return failure('EncodingError', message); }
  function current(state, generation) { return state.epoch === epoch && state.generation === generation; }
  function stop(state, reject) {
    if (state.timer !== null) clearTimeout(state.timer);
    state.timer = null;
    if (state.controller) state.controller.abort();
    state.controller = null; pending.delete(state);
    if (reject) for (const waiter of state.waiters.splice(0)) waiter.reject(encoding('Image source changed during decode'));
  }
  function emit(state, type) {
    const image = state.owner;
    let event;
    try { event = new Event(type); } catch (_) { event = { type }; }
    event.target = image; event.currentTarget = image; event.bubbles = false; event.cancelable = false;
    let immediate = false;
    event.stopImmediatePropagation = () => { immediate = true; };
    for (const listener of state.listeners.slice()) {
      if (immediate) break;
      if (listener.type !== type || !state.listeners.includes(listener)) continue;
      if (listener.once) state.listeners.splice(state.listeners.indexOf(listener), 1);
      try {
        if (typeof listener.callback === 'function') listener.callback.call(image, event);
        else if (listener.callback && typeof listener.callback.handleEvent === 'function') listener.callback.handleEvent(event);
      } catch (error) { setTimeout(() => { throw error; }, 0); }
    }
    event.currentTarget = null;
  }
  function finish(state, generation, pixels, error) {
    if (!current(state, generation)) return;
    state.timer = null; state.controller = null; pending.delete(state);
    state.pixels = pixels; state.status = error ? 'broken' : 'decoded'; state.error = error;
    track(state);
    for (const waiter of state.waiters.splice(0)) error ? waiter.reject(error) : waiter.resolve();
    emit(state, error ? 'error' : 'load');
  }
  function start(state, image, source, write) {
    if (state.epoch !== epoch) throw failure('InvalidStateError', 'Image belongs to a discarded document');
    stop(state, true); const generation = ++state.generation;
    state.owner = image; state.source = source; state.sourcePresent = write || !!source; state.pixels = null; state.error = null;
    state.status = source ? 'loading' : 'empty';
    let resolved;
    try { if (source) resolved = new URLCtor(source, globalThis.__oxiPageUrl || location.href); }
    catch (_) { resolved = null; }
    // Invalid URLs cannot be handed to Blitz's eager URL resolver, which
    // panics on them. The image state still reports the broken source.
    if (write && (!source || resolved)) state.setAttribute.call(image, 'src', source);
    else if (source && !resolved) state.removeAttribute.call(image, 'src');
    if (!source) return;
    pending.add(state);
    state.timer = setTimeout(() => {
      state.timer = null;
      if (!current(state, generation)) return;
      try {
        if (!resolved) throw encoding('Invalid image URL');
        if (resolved.protocol === 'data:') {
          finish(state, generation, decodePixels(dataBytes(resolved.href)), null);
        } else if (resolved.protocol === 'blob:') {
          const bytes = urls.get(resolved.href);
          if (!bytes) throw encoding('Blob URL is revoked or unknown');
          finish(state, generation, decodePixels(bytes), null);
        } else if (resolved.protocol === 'http:' || resolved.protocol === 'https:') {
          const base = new URLCtor(globalThis.__oxiPageUrl || location.href);
          if (base.origin === 'null' || resolved.origin !== base.origin) throw encoding('Cross-origin image loading is unsupported');
          const controller = new AbortController(); state.controller = controller;
          fetchImage(resolved.href, { credentials: 'same-origin', signal: controller.signal }).then(response => {
            if (!current(state, generation)) return null;
            if (!response.ok || new URLCtor(response.url).origin !== base.origin) throw encoding('Image HTTP failure or cross-origin redirect');
            return response.arrayBuffer();
          }).then(bytes => {
            if (current(state, generation) && bytes !== null) finish(state, generation, decodePixels(bytes), null);
          }).catch(error => { if (current(state, generation)) finish(state, generation, null, encoding(String(error.message || error))); });
        } else throw encoding('Unsupported image URL scheme');
      } catch (error) { finish(state, generation, null, encoding(String(error.message || error))); }
    }, 0);
  }
  function attach(image, node) {
    if (images.has(image)) return image;
    let state = typeof node === 'number' && byNode.has(node) ? byNode.get(node).deref() : undefined;
    if (!state || state.epoch !== epoch) {
      state = { epoch, node, generation: 0, source: '', sourcePresent: false, status: 'empty', pixels: null, error: null,
        timer: null, controller: null, waiters: [], listeners: [], owner: image,
        setAttribute: image.setAttribute, removeAttribute: image.removeAttribute };
      if (typeof node === 'number') byNode.set(node, new WeakRef(state));
    }
    images.set(image, state);
    const originalGet = image.getAttribute, originalSet = image.setAttribute, originalRemove = image.removeAttribute;
    const originalHas = image.hasAttribute;
    const originalAdd = image.addEventListener, originalRemoveListener = image.removeEventListener;
    function requireState() { if (state.epoch !== epoch) throw failure('InvalidStateError', 'Image belongs to a discarded document'); return state; }
    function dimension(name) { const value = originalGet.call(image, name); return value === null ? (state.pixels ? state.pixels[name] : 0) : Math.max(0, Number(value) >>> 0); }
    Object.defineProperties(image, {
      src: { configurable: true, enumerable: true, get() { const raw = state.source; try { return raw ? new URLCtor(raw, globalThis.__oxiPageUrl || location.href).href : ''; } catch (_) { return raw; } }, set(value) { start(requireState(), image, String(value), true); } },
      currentSrc: { configurable: true, get() { return state.status === 'decoded' ? image.src : ''; } },
      complete: { configurable: true, get() { return state.status !== 'loading'; } },
      naturalWidth: { configurable: true, get() { return state.pixels ? state.pixels.width : 0; } },
      naturalHeight: { configurable: true, get() { return state.pixels ? state.pixels.height : 0; } },
      width: { configurable: true, get() { return dimension('width'); }, set(value) { originalSet.call(image, 'width', String(Number(value) >>> 0)); } },
      height: { configurable: true, get() { return dimension('height'); }, set(value) { originalSet.call(image, 'height', String(Number(value) >>> 0)); } }
    });
    for (const type of ['load', 'error']) Object.defineProperty(image, 'on' + type, {
      configurable: true, get() { const entry = state.listeners.find(entry => entry.property === type); return entry ? entry.callback : null; },
      set(callback) {
        const existing = state.listeners.find(entry => entry.property === type);
        if (typeof callback !== 'function') { if (existing) state.listeners.splice(state.listeners.indexOf(existing), 1); }
        else if (existing) existing.callback = callback;
        else state.listeners.push({ type, callback, property: type, once: false });
      }
    });
    image.addEventListener = function(type, callback, options) {
      type = String(type);
      if (type !== 'load' && type !== 'error') return originalAdd.call(this, type, callback, options);
      if (callback === null || callback === undefined) return;
      const capture = typeof options === 'boolean' ? options : !!(options && options.capture);
      if (!state.listeners.some(entry => !entry.property && entry.type === type && entry.callback === callback && entry.capture === capture))
        state.listeners.push({ type, callback, capture, once: !!(options && typeof options === 'object' && options.once) });
    };
    image.removeEventListener = function(type, callback, options) {
      type = String(type);
      if (type !== 'load' && type !== 'error') return originalRemoveListener.call(this, type, callback, options);
      const capture = typeof options === 'boolean' ? options : !!(options && options.capture);
      const index = state.listeners.findIndex(entry => !entry.property && entry.type === type && entry.callback === callback && entry.capture === capture);
      if (index >= 0) state.listeners.splice(index, 1);
    };
    image.getAttribute = function(name) { return String(name).toLowerCase() === 'src' ? (state.sourcePresent ? state.source : null) : originalGet.call(this, name); };
    image.hasAttribute = function(name) { return String(name).toLowerCase() === 'src' ? state.sourcePresent : originalHas.call(this, name); };
    image.setAttribute = function(name, value) { if (String(name).toLowerCase() === 'src') start(requireState(), image, String(value), true); else originalSet.call(this, name, value); };
    image.removeAttribute = function(name) { if (String(name).toLowerCase() === 'src') { start(requireState(), image, '', false); originalRemove.call(this, name); } else originalRemove.call(this, name); };
    image.decode = function() {
      if (state.epoch !== epoch) return Promise.reject(encoding('Image belongs to a discarded document'));
      if (state.status === 'decoded') return Promise.resolve();
      if (state.status !== 'loading') return Promise.reject(state.error || encoding('Image has no decodable source'));
      return new Promise((resolve, reject) => state.waiters.push({ resolve, reject }));
    };
    Object.setPrototypeOf(image, Image.prototype);
    const initial = originalGet.call(image, 'src');
    if (initial !== null) state.sourcePresent = true;
    if (initial && !state.source) start(state, image, initial, false);
    track(state);
    return image;
  }
  function Image(width, height) {
    if (!new.target) throw new TypeError('Image requires new');
    const image = document.createElement('img'); attach(image, image.__nodeId);
    if (width !== undefined) image.width = width;
    if (height !== undefined) image.height = height;
    return image;
  }
  Image.prototype = Object.create(globalThis.HTMLElement ? HTMLElement.prototype : Object.prototype);
  Object.defineProperty(Image.prototype, 'constructor', { value: Image, writable: true, configurable: true });
  URLCtor.createObjectURL = function(blob) {
    const bytes = blobBytes(blob);
    if (urls.size >= 4096 || urlBytes + bytes.byteLength > LIMIT) throw new RangeError('Blob URL registry exceeds its limit');
    const url = mintURL(); urls.set(url, bytes); urlBytes += bytes.byteLength; return url;
  };
  URLCtor.revokeObjectURL = function(value) {
    const url = String(value), bytes = urls.get(url);
    if (bytes) { urlBytes -= bytes.byteLength; urls.delete(url); }
  };
  Object.defineProperties(globalThis, {
    __oxiAttachImage: { value: attach },
    __oxiInstallImageLifecycle: { value: installLifecycle },
    __oxiSweepImages: { value() {
      for (const [node, state] of connected) {
        if (state.epoch !== epoch || !isConnected(state.owner)) connected.delete(node);
      }
    } },
    __oxiResetImages: { value() {
      const abandoned = pending; pending = new Set(); epoch++;
      byNode.clear(); connected.clear(); urls.clear(); urlBytes = 0;
      for (const state of abandoned) { state.waiters.length = 0; stop(state, false); }
    } },
    __oxiImagePixels: { value(image) {
      const state = images.get(image); if (!state) return null;
      if (state.epoch !== epoch || state.status !== 'decoded' || !state.pixels) throw failure('InvalidStateError', 'Image is not decoded');
      return { width: state.pixels.width, height: state.pixels.height, data: state.pixels.data.slice(0) };
    } }
  });
  globalThis.Image = Image; globalThis.HTMLImageElement = Image;
  installLifecycle();
  if (globalThis.window) { window.Image = Image; window.HTMLImageElement = Image; }
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn retained_image_release(clear: &str) {
        use ::image::ImageEncoder;
        let rgba = [255, 0, 0, 255, 0, 255, 0, 128];
        let mut png = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&rgba, 2, 1, ::image::ExtendedColorType::Rgba8)
            .unwrap();
        let bytes = serde_json::to_string(&png).unwrap();
        let output = Arc::new(RwLock::new(Vec::new()));
        let snapshot = Arc::new(RwLock::new(None));
        let mutations = Arc::new(RwLock::new(Vec::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        let document = Rc::new(RefCell::new(Some(
            RenderDocument::from_html(
                "<html><body><div id='container'></div></body></html>",
                Some("https://image.example/"),
                Viewport::default(),
            )
            .unwrap(),
        )));
        let (mut ctx, queue) = create_context(
            &output,
            &snapshot,
            &mutations,
            &dirty,
            (320, 240),
            "https://image.example/",
            "Image GC test",
            &Arc::new(RwLock::new(None)),
            &Arc::new(RwLock::new(None)),
            &document,
            false,
        );
        ctx.eval(Source::from_bytes(format!(r#"
          (() => {{
            const image = new Image(), url = URL.createObjectURL(new Blob([new Uint8Array({bytes})]));
            image.id = 'retained'; image.onload = () => URL.revokeObjectURL(url);
            image.src = url; document.querySelector('#container').appendChild(image);
            globalThis.attachedRef = new WeakRef(image);
            const detached = new Image(), other = URL.createObjectURL(new Blob([new Uint8Array({bytes})]));
            detached.onload = () => URL.revokeObjectURL(other); detached.src = other;
            globalThis.detachedRef = new WeakRef(detached);
          }})();
        "#).as_bytes())).unwrap();
        drain_timers(&queue, &mut ctx);
        ctx.clear_kept_objects();
        boa_gc::force_collect();
        let detached_gone = ctx
            .eval(Source::from_bytes("detachedRef.deref() === undefined"))
            .unwrap();
        assert_eq!(detached_gone.as_boolean(), Some(true));
        let queried = ctx
            .eval(Source::from_bytes("document.querySelector('#retained')"))
            .unwrap();
        let (width, height, pixels) = source_pixels(&queried, &mut ctx).unwrap().unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(pixels, rgba);
        ctx.eval(Source::from_bytes(clear)).unwrap();
        drop(queried);
        ctx.clear_kept_objects();
        boa_gc::force_collect();
        let attached_gone = ctx
            .eval(Source::from_bytes("attachedRef.deref() === undefined"))
            .unwrap();
        assert_eq!(attached_gone.as_boolean(), Some(true));
    }

    #[test]
    fn connected_bitmap_survives_wrapper_gc_but_detached_image_is_collectible() {
        retained_image_release("document.querySelector('#retained').remove()");
    }

    #[test]
    fn connected_bitmap_is_released_when_ancestor_inner_html_is_cleared() {
        retained_image_release("document.querySelector('#container').innerHTML = ''");
    }

    #[test]
    fn connected_bitmap_is_released_when_ancestor_text_content_is_cleared() {
        retained_image_release("document.querySelector('#container').textContent = ''");
    }
}
