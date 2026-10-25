//! WebGL contexts for canvas elements, backed by ANGLE (`oxibrowser-render`
//! `gl` module) and exposed to Boa.
//!
//! Architecture (per `docs/research/2026-10-25-webgl-open-source.md`):
//!
//! * **JS thread only.** Every GL context is created, used, and destroyed on
//!   the `oxibrowser-js` thread — the same thread that owns `RenderDocument`
//!   and the boa `Context`. No command marshaling; ANGLE validates GL
//!   semantics under `EGL_CONTEXT_WEBGL_COMPATIBILITY_ANGLE`.
//! * **Registry.** Thread-local map keyed by `(JS frame id, canvas node id)`.
//!   Cleared on `SetDocument` like the listener registry, which also
//!   guarantees EGL teardown happens on the owning thread (GlContext::Drop).
//! * **Identity & pinning.** One context per canvas node. Requesting `webgl`
//!   after `webgl2` (or vice versa) returns `null`; requesting the same kind
//!   again returns the same JS object.
//! * **Ownership.** Every GL wrapper object records its owning context
//!   handle; use from another context is INVALID_OPERATION.
//!
//! Presentation: readback into the canvas node's raster-image slot before
//! screenshots (`flush_canvas`); `toDataURL` reads without presenting.

pub mod api;
pub mod canvas;
pub mod flush;
pub mod types;

#[cfg(all(test, feature = "webgl"))]
mod session_tests;

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::JsValue;
use boa_engine::object::JsObject;
use oxibrowser_render::gl::{GlContext, GlError};

use crate::js::runtime::active_context_id;
pub use types::{CtxKind, JsContextAttrs, Resource, ResourceKind};

/// Unique frontend-context handle (per JS thread).
static NEXT_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// One live WebGL context: native state + the rooted JS object.
pub struct RegistryEntry {
    /// Stable handle used by JS wrapper objects for ownership checks.
    pub handle: u64,
    /// Canvas node id in this frame's RenderDocument.
    pub node_id: usize,
    pub kind: CtxKind,
    /// The context object JS received from `getContext` — identity anchor.
    pub js: JsObject,
    pub ctx: FrontContext,
}

/// Frontend GL state for one context.
pub struct FrontContext {
    pub backend: GlContext,
    pub attrs: JsContextAttrs,
    /// (vendor, renderer) reported via `getParameter(UNMASKED_*_WEBGL)` —
    /// populated from the stealth profile so spoofed identity stays coherent.
    pub identity: (String, String),
    /// res id → resource. Every GL object this context mints.
    pub resources: HashMap<u32, Resource>,
    next_res: u32,
    /// Error queue per spec (`getError` pops one at a time).
    errors: std::collections::VecDeque<u32>,
    // ── Frontend state mirrors (owned by the js/webgl binding layer) ──
    /// `UNPACK_FLIP_Y_WEBGL` / `UNPACK_PREMULTIPLY_ALPHA_WEBGL` — pure WebGL
    /// pixel-store state with no GLES equivalent; applied as a CPU transform
    /// in the texImage2D path.
    pub unpack_flip_y: bool,
    pub unpack_premultiply: bool,
    /// Minimal binding mirror so `getParameter` can answer *_BINDING /
    /// CURRENT_PROGRAM queries without GL round trips. Values are frontend
    /// resource ids (`FrontContext.resources` keys), not GL names.
    pub bound_array_buffer: Option<u32>,
    pub bound_element_array_buffer: Option<u32>,
    pub bound_framebuffer: Option<u32>,
    pub bound_renderbuffer: Option<u32>,
    pub bound_texture_2d: Option<u32>,
    pub bound_texture_cube_map: Option<u32>,
    pub used_program: Option<u32>,
    pub active_texture_unit: u32,
}

impl FrontContext {
    pub fn new(backend: GlContext, attrs: JsContextAttrs, identity: (String, String)) -> Self {
        Self {
            backend,
            attrs,
            identity,
            resources: HashMap::new(),
            next_res: 1,
            errors: std::collections::VecDeque::new(),
            unpack_flip_y: false,
            unpack_premultiply: false,
            bound_array_buffer: None,
            bound_element_array_buffer: None,
            bound_framebuffer: None,
            bound_renderbuffer: None,
            bound_texture_2d: None,
            bound_texture_cube_map: None,
            used_program: None,
            active_texture_unit: 0,
        }
    }

    pub fn alloc_resource(&mut self, kind: ResourceKind, handle: u32) -> u32 {
        let id = self.next_res;
        self.next_res += 1;
        self.resources.insert(
            id,
            Resource {
                kind,
                handle,
                deleted: false,
            },
        );
        id
    }

    pub fn resource(&self, id: u32) -> Option<Resource> {
        self.resources.get(&id).copied()
    }

    pub fn mark_deleted(&mut self, id: u32) {
        if let Some(r) = self.resources.get_mut(&id) {
            r.deleted = true;
        }
    }

    pub fn push_error(&mut self, code: u32) {
        self.errors.push_back(code);
    }

    pub fn pop_error(&mut self) -> u32 {
        self.errors.pop_front().unwrap_or(types::NO_ERROR)
    }

    /// Resizes the drawing surface (canvas width/height attribute change).
    /// Per WebGL resize semantics the drawing buffer is cleared (handled by
    /// the backend surface swap) and GL object state survives.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GlError> {
        self.backend.resize(width, height)
    }
}

thread_local! {
    /// All live WebGL contexts on this JS thread, keyed by (frame, canvas node).
    static REGISTRY: RefCell<HashMap<(u32, usize), RegistryEntry>> = RefCell::new(HashMap::new());

    /// EGL context currently bound on this thread (0 = none). Lets hot paths
    /// skip redundant `eglMakeCurrent` round trips — a framework issuing
    /// thousands of calls per frame must not pay context switches per call.
    static CURRENT_GL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Drops all WebGL contexts belonging to the current JS frame. Called from
/// `SetDocument` next to the listener-registry cleanup.
pub fn reset_for_document() {
    CURRENT_GL.set(0);
    clear_canvas_dims_for_current_frame();
    REGISTRY.with(|r| r.borrow_mut().clear());
}

fn with_entry_mut<R>(key: (u32, usize), f: impl FnOnce(&mut RegistryEntry) -> R) -> Option<R> {
    REGISTRY.with(|r| {
        let mut m = r.borrow_mut();
        m.get_mut(&key).map(f)
    })
}

/// Looks up an entry read-only.
pub fn with_entry<R>(key: (u32, usize), f: impl FnOnce(&RegistryEntry) -> R) -> Option<R> {
    REGISTRY.with(|r| r.borrow().get(&key).map(f))
}

/// The registry key of the canvas this JS frame is executing against.
pub fn key_for(node_id: usize) -> (u32, usize) {
    (active_context_id(), node_id)
}

// Canvas element dimensions, per canvas node. The DOM `width`/`height`
// attributes are the *initial* size; live changes are tracked here because
// blitz-dom's mutator does not persist `width`/`height` writes on canvas
// elements (verified in isolation — `data-*`/other elements round-trip
// fine). Layout keeps using the original attributes; the GL drawing buffer
// follows this override.
thread_local! {
    static CANVAS_DIMS: RefCell<HashMap<(u32, usize), (u32, u32)>> =
        RefCell::new(HashMap::new());
}

/// Records the canvas element size (from attribute/IDL writes).
pub fn set_canvas_dims(node_id: usize, width: u32, height: u32) {
    let key = (active_context_id(), node_id);
    CANVAS_DIMS.with(|m| m.borrow_mut().insert(key, (width, height)));
}

/// Reads the recorded canvas size, if any.
pub fn canvas_dims(node_id: usize) -> Option<(u32, u32)> {
    let key = (active_context_id(), node_id);
    CANVAS_DIMS.with(|m| m.borrow().get(&key).copied())
}

fn clear_canvas_dims_for_current_frame() {
    let frame = active_context_id();
    CANVAS_DIMS.with(|m| m.borrow_mut().retain(|(fid, _), _| *fid != frame));
}

/// `canvas.getContext(kind, attrs)` — the native entry behind the JS binding.
/// Enforces identity (same JS object), type pinning (second kind → null), and
/// creation failure → `null` with a logged reason (spec: unsupported returns
/// null; we surface the exact cause). `js_obj_builder` receives the handle and
/// the canvas back-reference; `width`/`height` are the canvas attribute sizes.
#[allow(clippy::too_many_arguments)]
pub fn get_context(
    node_id: usize,
    kind_str: &str,
    attrs: Option<JsContextAttrs>,
    identity: (String, String),
    width: u32,
    height: u32,
    js_obj_builder: impl FnOnce(u64, JsValue) -> JsObject,
    canvas: JsValue,
) -> JsValue {
    let Some(kind) = CtxKind::from_type_str(kind_str) else {
        return JsValue::null();
    };
    let key = key_for(node_id);

    if let Some(existing) = with_entry(key, |e| (e.handle, e.kind)) {
        if existing.1 == kind
            && let Some(js) = with_entry(key, |e| e.js.clone())
        {
            return JsValue::from(js);
        }
        // A context already exists with the other kind → null (type pinned).
        return JsValue::null();
    }

    let attrs = attrs.unwrap_or_default();
    let handle = NEXT_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (cw, ch) = (
        width.clamp(1, types::MAX_DIM),
        height.clamp(1, types::MAX_DIM),
    );
    match GlContext::new(
        kind.client_version(),
        cw,
        ch,
        types::to_render_attrs(&attrs),
    ) {
        Ok(backend) => {
            let js = js_obj_builder(handle, canvas);
            let entry = RegistryEntry {
                handle,
                node_id,
                kind,
                js: js.clone(),
                ctx: FrontContext::new(backend, attrs, identity),
            };
            REGISTRY.with(|r| r.borrow_mut().insert(key, entry));
            JsValue::from(js)
        }
        Err(e) => {
            tracing::warn!(kind = kind_str, error = %e, "WebGL context creation failed; getContext returns null");
            JsValue::null()
        }
    }
}

/// Resizes the context's drawing surface after a canvas width/height change.
/// No-op when the canvas has no context or the size is unchanged.
pub fn resize(node_id: usize, width: u32, height: u32) {
    let key = (active_context_id(), node_id);
    let _ = with_entry_mut(key, |e| e.ctx.resize(width, height));
}

/// All live entries in this frame: (handle, node_id).
pub fn entries() -> Vec<(u64, usize)> {
    let frame = active_context_id();
    REGISTRY.with(|r| {
        r.borrow()
            .iter()
            .filter(|((fid, _), _)| *fid == frame)
            .map(|((_, nid), e)| (e.handle, *nid))
            .collect()
    })
}

/// Resolves a registry entry by context handle.
pub fn find_by_handle(handle: u64) -> Option<(u32, usize)> {
    REGISTRY.with(|r| {
        r.borrow()
            .iter()
            .find(|(_, e)| e.handle == handle)
            .map(|(k, _)| *k)
    })
}

/// Runs `f` with the context's GL context current on this thread. `Err` when
/// the entry is stale or the GL context could not bind (lost) — callers
/// enqueue CONTEXT_LOST_WEBGL themselves where spec-visible.
///
/// Binding is sticky: consecutive calls on the same context skip
/// `eglMakeCurrent` entirely. Use [`release_current_gl`] before operations
/// that must not hold a binding (document teardown).
pub fn with_gl<R>(
    handle: u64,
    f: impl FnOnce(&mut RegistryEntry, &glow::Context) -> R,
) -> Result<R, GlError> {
    let key = find_by_handle(handle).ok_or(GlError::Lost)?;
    let loader =
        oxibrowser_render::gl::loader().map_err(|e| GlError::Unsupported(e.to_string()))?;
    with_entry_mut(key, |e| {
        if CURRENT_GL.get() != handle {
            e.ctx.backend.make_current()?;
            CURRENT_GL.set(handle);
        }
        Ok(f(e, loader))
    })
    .ok_or(GlError::Lost)?
}

/// Unbinds `handle` if it is the current GL context on this thread.
pub fn release_current_gl(handle: u64) {
    if CURRENT_GL.get() != handle {
        return;
    }
    if let Some(key) = find_by_handle(handle) {
        let _ = with_entry_mut(key, |e| e.ctx.backend.make_not_current());
    }
    CURRENT_GL.set(0);
}

/// Reads back one context's drawing buffer, top-down RGBA8.
/// `present=true` clears afterwards unless `preserveDrawingBuffer` (WebGL
/// compositing semantics); `toDataURL` uses `present=false`.
pub fn readback(handle: u64, present: bool) -> Option<(u32, u32, Vec<u8>)> {
    with_gl(handle, |e, gl| {
        let (w, h) = (e.ctx.backend.width, e.ctx.backend.height);
        (w, h, e.ctx.backend.read_pixels_rgba(gl, present))
    })
    .ok()
}

/// Readback converted to **straight (unpremultiplied) alpha**, which is what
/// blitz raster images and PNG encoding expect. WebGL drawing buffers are
/// premultiplied by default (`premultipliedAlpha: true`); `alpha: false`
/// contexts already have opaque alpha so conversion is a no-op.
pub fn readback_straight(node_id: usize, present: bool) -> Option<(u32, u32, Vec<u8>)> {
    let key = key_for(node_id);
    let (premul, handle) = with_entry(key, |e| (e.ctx.attrs.premultiplied_alpha, e.handle))?;
    let (w, h, mut rgba) = readback(handle, present)?;
    if premul {
        premultiplied_to_straight(&mut rgba);
    }
    Some((w, h, rgba))
}

/// In-place premultiplied → straight conversion, per byte quartet RGBA8.
pub fn premultiplied_to_straight(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a == 255 || a == 0 {
            continue;
        }
        for c in &mut px[..3] {
            let v = (*c as u32 * 255 + a / 2) / a;
            *c = v.min(255) as u8;
        }
    }
}

/// Feature probe for console diagnostics — true when the ANGLE display has
/// the WebGL compatibility extension. Initializes the display on first call.
pub fn is_available() -> bool {
    oxibrowser_render::gl::display_info()
        .map(|i| i.webgl_compatible())
        .unwrap_or(false)
}

/// Hidden property names on wrapper objects (codebase convention: `__`-prefixed).
pub mod props {
    use boa_engine::JsString;
    pub const CTX: &str = "__webglCtx";
    pub const RES: &str = "__webglRes";
    pub const KIND: &str = "__webglKind";

    pub fn ctx() -> JsString {
        JsString::from(CTX)
    }
    pub fn res() -> JsString {
        JsString::from(RES)
    }
    pub fn kind() -> JsString {
        JsString::from(KIND)
    }
}

/// Re-exported for the runtime Capture flush.
pub use flush::flush_canvas;
