//! The Boa binding layer for WebGL contexts: the object returned by
//! `canvas.getContext("webgl" | "webgl2")`.
//!
//! Layout (per the module docs in `mod.rs`):
//!
//! * Every method is an **own property** — this codebase does not use
//!   prototype chains. Constants are own number properties set from the
//!   [`CONSTANTS`] / [`CONSTANTS_WEBGL2`] tables.
//! * Dispatch: each method reads the hidden `__webglCtxHandle` off `this`,
//!   enters [`super::with_gl`], and returns `undefined` on `GlError`
//!   (lost/stale contexts are silent per WebGL lost-context semantics).
//!   Inside `with_gl`, `entry.handle` always equals the dispatched handle,
//!   so wrappers minted during a call record `entry.handle` as owner.
//! * Resource wrappers (hidden `__webglCtx`/`__webglRes`/`__webglKind` props)
//!   carry the owning context handle; cross-context use, kind mismatches and
//!   deleted objects are `INVALID_OPERATION`, unknown ids `INVALID_VALUE`
//!   ([`types::validate_resource_use`]).
//! * WebGL-level problems enqueue a frontend error
//!   ([`super::FrontContext::push_error`]); wrong JS argument types throw
//!   `TypeError`.

use std::num::NonZeroU32;

use boa_engine::object::builtins::{
    JsArray, JsArrayBuffer, JsFloat32Array, JsInt32Array, JsTypedArray,
};
use boa_engine::object::{FunctionObjectBuilder, JsObject};
use boa_engine::property::PropertyDescriptor;
use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction, js_string,
};
use glow::HasContext;

use super::types::{self, CtxKind, ResourceKind};
use super::{RegistryEntry, props};

/// Hidden prop on the context object carrying the frontend context handle.
const CTX_HANDLE_PROP: &str = "__webglCtxHandle";

/// Extensions exposed by `getExtension` when ANGLE advertises them. In WebGL2
/// every one of these is core, so `getExtension` reports `null` there.
const EXTENSION_ALLOWLIST: &[&str] = &[
    "OES_element_index_uint",
    "OES_standard_derivatives",
    "EXT_blend_minmax",
    "EXT_frag_depth",
    "EXT_shader_texture_lod",
    "OES_texture_float",
    "OES_texture_half_float",
    "OES_texture_float_linear",
    "OES_texture_half_float_linear",
    "OES_vertex_array_object",
];

// ─── JS helpers ─────────────────────────────────────────────────────────────

/// The frontend context handle stashed on `this` by [`build_context_object`].
fn this_handle(this: &JsValue, cx: &mut Context) -> Option<u64> {
    let obj = this.as_object()?;
    obj.get(JsString::from(CTX_HANDLE_PROP), cx)
        .ok()?
        .as_number()
        .map(|n| n as u64)
}

/// Builds a resource wrapper object (hidden props per `props`).
fn make_wrapper(cx: &mut Context, ctx_handle: u64, res_id: u32, kind: ResourceKind) -> JsObject {
    let obj = JsObject::with_object_proto(cx.intrinsics());
    let _ = obj.set(
        JsString::from(props::CTX),
        JsValue::from(ctx_handle as f64),
        false,
        cx,
    );
    let _ = obj.set(
        JsString::from(props::RES),
        JsValue::from(res_id as f64),
        false,
        cx,
    );
    let _ = obj.set(
        JsString::from(props::KIND),
        JsValue::from(JsString::from(kind.type_name())),
        false,
        cx,
    );
    obj
}

fn kind_from_type_name(name: &str) -> Option<ResourceKind> {
    match name {
        "WebGLBuffer" => Some(ResourceKind::Buffer),
        "WebGLShader" => Some(ResourceKind::Shader),
        "WebGLProgram" => Some(ResourceKind::Program),
        "WebGLTexture" => Some(ResourceKind::Texture),
        "WebGLFramebuffer" => Some(ResourceKind::Framebuffer),
        "WebGLRenderbuffer" => Some(ResourceKind::Renderbuffer),
        "WebGLVertexArrayObject" => Some(ResourceKind::VertexArray),
        "WebGLSampler" => Some(ResourceKind::Sampler),
        "WebGLQuery" => Some(ResourceKind::Query),
        "WebGLUniformLocation" => Some(ResourceKind::UniformLocation),
        _ => None,
    }
}

/// Parses a resource wrapper: `(owning ctx handle, frontend res id, kind)`.
fn unwrap_wrapper(v: &JsValue, cx: &mut Context) -> Option<(u64, u32, ResourceKind)> {
    let obj = v.as_object()?;
    let owner = obj.get(JsString::from(props::CTX), cx).ok()?.as_number()? as u64;
    let id = obj.get(JsString::from(props::RES), cx).ok()?.as_number()? as u32;
    let kind_name = obj
        .get(JsString::from(props::KIND), cx)
        .ok()?
        .as_string()?
        .to_std_string_escaped();
    let kind = kind_from_type_name(&kind_name)?;
    Some((owner, id, kind))
}

fn type_err(msg: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(msg.into()).into()
}

/// Resolves a wrapper argument to a usable native GL name, enforcing
/// ownership / kind / deletion ([`types::validate_resource_use`]).
///
/// * `Ok(Some(handle))` — usable.
/// * `Ok(None)` — a GL error was already enqueued on the context.
/// * `Err(..)` — `TypeError` (argument was not a wrapper object at all).
fn resolve_resource(
    entry: &mut RegistryEntry,
    this_handle: u64,
    v: &JsValue,
    cx: &mut Context,
    expected: ResourceKind,
) -> Result<Option<u32>, JsError> {
    let Some((owner, id, _kind)) = unwrap_wrapper(v, cx) else {
        return Err(type_err(format!(
            "expected a {} object",
            expected.type_name()
        )));
    };
    // Ownership first: an object from another context is INVALID_OPERATION
    // even though its id is unknown to this context's table.
    if owner != this_handle {
        entry.ctx.push_error(types::INVALID_OPERATION);
        return Ok(None);
    }
    let fctx = &mut entry.ctx;
    let Some(res) = fctx.resource(id) else {
        fctx.push_error(types::INVALID_VALUE);
        return Ok(None);
    };
    if let Some(err) = types::validate_resource_use(&res, expected, owner, this_handle) {
        fctx.push_error(err);
        return Ok(None);
    }
    Ok(Some(res.handle))
}

/// Mints a fresh resource in the frontend registry and wraps it for JS.
fn new_resource(
    cx: &mut Context,
    entry: &mut RegistryEntry,
    ctx_handle: u64,
    kind: ResourceKind,
    gl_handle: u32,
) -> JsValue {
    let id = entry.ctx.alloc_resource(kind, gl_handle);
    JsValue::from(make_wrapper(cx, ctx_handle, id, kind))
}

/// `getParameter` answer for binding-mirror pnames: wrapper or `null`.
fn binding_wrapper(cx: &mut Context, handle: u64, id: Option<u32>, kind: ResourceKind) -> JsValue {
    match id {
        Some(id) => JsValue::from(make_wrapper(cx, handle, id, kind)),
        None => JsValue::null(),
    }
}

fn nz(id: u32) -> NonZeroU32 {
    // GL object names are allocated from 1; 0 is reserved for "no object".
    NonZeroU32::new(id).expect("GL object names are never 0")
}

// ─── Buffer view extraction ─────────────────────────────────────────────────

/// Extracts the viewed bytes of an `ArrayBufferView` (or whole `ArrayBuffer`).
/// `Ok(None)` when the value is null/undefined. Duck-types plain
/// length-indexed objects as a fallback, mirroring `wasm.rs::extract_bytes`.
///
/// The bytes are copied out so no GC borrow is held across GL calls.
fn buffer_source_bytes(v: &JsValue, cx: &mut Context) -> Result<Option<Vec<u8>>, JsError> {
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    let Some(obj) = v.as_object() else {
        return Err(type_err("expected an ArrayBufferView or null"));
    };
    if let Ok(ta) = JsTypedArray::from_object(obj.clone()) {
        let offset = ta.byte_offset(cx)?;
        let len = ta.byte_length(cx)?;
        let buf_v = ta.buffer(cx)?;
        let buf_obj = buf_v
            .as_object()
            .ok_or_else(|| type_err("typed array has no backing buffer"))?;
        let buf = JsArrayBuffer::from_object(buf_obj)?;
        let Some(data) = buf.data() else {
            return Err(type_err("typed array buffer is detached"));
        };
        let end = offset + len;
        if end > data.len() {
            return Err(type_err("typed array view exceeds its buffer"));
        }
        return Ok(Some(data[offset..end].to_vec()));
    }
    if let Ok(buf) = JsArrayBuffer::from_object(obj.clone()) {
        let Some(data) = buf.data() else {
            return Err(type_err("ArrayBuffer is detached"));
        };
        return Ok(Some(data.to_vec()));
    }
    // Duck-typed fallback: an Array (or array-like) of numbers.
    let Some(len) = obj.get(js_string!("length"), cx)?.as_number() else {
        return Err(type_err("expected a buffer source"));
    };
    let n = len as usize;
    let mut bytes = Vec::with_capacity(n);
    for i in 0..n {
        bytes.push(obj.get(i as u32, cx)?.to_number(cx)? as u8);
    }
    Ok(Some(bytes))
}

/// `Float32Array` or a JS sequence of numbers → `Vec<f32>` (WebGL spec allows
/// both for `*fv` methods). `TypeError` otherwise.
fn f32_sequence(v: &JsValue, cx: &mut Context) -> Result<Vec<f32>, JsError> {
    if let Some(obj) = v.as_object() {
        if let Ok(ta) = JsFloat32Array::from_object(obj.clone()) {
            let n = ta.length(cx)?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(ta.get(i, cx)?.as_number().unwrap_or(0.0) as f32);
            }
            return Ok(out);
        }
        if let Some(len) = obj.get(js_string!("length"), cx)?.as_number() {
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len as usize {
                out.push(obj.get(i as u32, cx)?.to_number(cx)? as f32);
            }
            return Ok(out);
        }
    }
    Err(type_err("expected a Float32Array or a number sequence"))
}

/// `Int32Array` or a JS sequence of numbers → `Vec<i32>`.
fn i32_sequence(v: &JsValue, cx: &mut Context) -> Result<Vec<i32>, JsError> {
    if let Some(obj) = v.as_object() {
        if let Ok(ta) = JsInt32Array::from_object(obj.clone()) {
            let n = ta.length(cx)?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(ta.get(i, cx)?.as_number().unwrap_or(0.0) as i32);
            }
            return Ok(out);
        }
        if let Some(len) = obj.get(js_string!("length"), cx)?.as_number() {
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len as usize {
                out.push(obj.get(i as u32, cx)?.to_number(cx)? as i32);
            }
            return Ok(out);
        }
    }
    Err(type_err("expected an Int32Array or a number sequence"))
}

/// Copies `bytes` into a typed array's backing store at its `byte_offset`.
/// Returns `Ok(false)` when the destination is too small (callers turn that
/// into `INVALID_OPERATION` per the readPixels spec).
fn write_bytes_into_view(v: &JsValue, bytes: &[u8], cx: &mut Context) -> Result<bool, JsError> {
    let Some(obj) = v.as_object() else {
        return Err(type_err("expected an ArrayBufferView"));
    };
    let ta = JsTypedArray::from_object(obj)?;
    let offset = ta.byte_offset(cx)?;
    let buf_v = ta.buffer(cx)?;
    let buf_obj = buf_v
        .as_object()
        .ok_or_else(|| type_err("typed array has no backing buffer"))?;
    let buf = JsArrayBuffer::from_object(buf_obj)?;
    let Some(mut data) = buf.data_mut() else {
        return Err(type_err("typed array buffer is detached"));
    };
    let end = offset + bytes.len();
    if end > data.len() {
        return Ok(false);
    }
    data[offset..end].copy_from_slice(bytes);
    Ok(true)
}

// ─── Unpack transforms (WebGL-only pixelStorei state) ───────────────────────

/// Applies `UNPACK_FLIP_Y_WEBGL` / `UNPACK_PREMULTIPLY_ALPHA_WEBGL` on the
/// CPU before upload. GLES has no equivalent pixel-store enums, so the
/// frontend mirrors the flags and rewrites the client bytes — the CPU path.
/// Restricted to RGBA/UNSIGNED_BYTE where row and pixel layout are
/// unambiguous; other format/type combos upload untransformed (ANGLE still
/// validates the upload itself under WebGL compatibility mode).
fn apply_unpack_transform(
    bytes: &mut [u8],
    width: i32,
    height: i32,
    format: u32,
    ty: u32,
    flip_y: bool,
    premultiply: bool,
) {
    if (!flip_y && !premultiply) || ty != glow::UNSIGNED_BYTE || format != glow::RGBA {
        return;
    }
    let (w, h) = (width.max(0) as usize, height.max(0) as usize);
    let bpp = 4usize;
    let row = w * bpp;
    if w == 0 || h == 0 || bytes.len() < row * h {
        return;
    }
    if flip_y {
        for y in 0..h / 2 {
            let top = y * row;
            let bottom = (h - 1 - y) * row;
            for x in 0..row {
                bytes.swap(top + x, bottom + x);
            }
        }
    }
    if premultiply {
        for px in bytes.chunks_exact_mut(bpp) {
            let a = px[3] as u32;
            if a < 255 {
                px[0] = ((px[0] as u32 * a + 127) / 255) as u8;
                px[1] = ((px[1] as u32 * a + 127) / 255) as u8;
                px[2] = ((px[2] as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

/// Bytes-per-pixel for the legacy 6/7-arg tex*Image2D overloads, where the
/// dimensions are derived from the data length.
fn bytes_per_pixel(format: u32, ty: u32) -> Option<usize> {
    if ty != glow::UNSIGNED_BYTE {
        return None;
    }
    match format {
        glow::RGBA => Some(4),
        glow::RGB => Some(3),
        glow::ALPHA | glow::LUMINANCE => Some(1),
        glow::LUMINANCE_ALPHA => Some(2),
        _ => None,
    }
}

/// Square-dimension derivation for the legacy overloads: the byte length must
/// decompose exactly into `side × side` pixels. `None` after queueing
/// `INVALID_OPERATION` when it does not.
fn square_dims(bytes: &[u8], entry: &mut RegistryEntry, format: u32, ty: u32) -> Option<i32> {
    let bpp = bytes_per_pixel(format, ty)?;
    if bytes.is_empty() || !bytes.len().is_multiple_of(bpp) {
        entry.ctx.push_error(types::INVALID_OPERATION);
        return None;
    }
    let px = (bytes.len() / bpp) as i32;
    let side = (px as f64).sqrt() as i32;
    if side == 0 || side * side != px {
        entry.ctx.push_error(types::INVALID_OPERATION);
        return None;
    }
    Some(side)
}

// ─── Argument helpers ───────────────────────────────────────────────────────

fn num_at(args: &[JsValue], i: usize) -> Result<f64, JsError> {
    args.get_or_undefined(i)
        .as_number()
        .ok_or_else(|| type_err(format!("argument {i} must be a number")))
}

fn i32_at(args: &[JsValue], i: usize) -> Result<i32, JsError> {
    Ok(num_at(args, i)? as i32)
}

fn u32_at(args: &[JsValue], i: usize) -> Result<u32, JsError> {
    Ok(num_at(args, i)? as u32)
}

fn f32_at(args: &[JsValue], i: usize) -> Result<f32, JsError> {
    Ok(num_at(args, i)? as f32)
}

fn str_at(args: &[JsValue], i: usize) -> Result<String, JsError> {
    args.get_or_undefined(i)
        .as_string()
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| type_err(format!("argument {i} must be a string")))
}

/// WebGL-spec element size for `drawElements` types (bytes per index).
fn index_type_size(ty: u32) -> Option<i32> {
    match ty {
        glow::UNSIGNED_BYTE => Some(1),
        glow::UNSIGNED_SHORT => Some(2),
        glow::UNSIGNED_INT => Some(4),
        _ => None,
    }
}

// ─── Constants ──────────────────────────────────────────────────────────────

/// The WebGL 1.0 constant table (spec values), set as own number properties.
const CONSTANTS: &[(&str, f64)] = &[
    // Clear buffers and misc bits
    ("DEPTH_BUFFER_BIT", 0x0000_0100 as f64),
    ("STENCIL_BUFFER_BIT", 0x0000_0400 as f64),
    ("COLOR_BUFFER_BIT", 0x0000_4000 as f64),
    // Begin/end modes
    ("POINTS", 0.0),
    ("LINES", 1.0),
    ("LINE_LOOP", 2.0),
    ("LINE_STRIP", 3.0),
    ("TRIANGLES", 4.0),
    ("TRIANGLE_STRIP", 5.0),
    ("TRIANGLE_FAN", 6.0),
    // Blending factors
    ("ZERO", 0.0),
    ("ONE", 1.0),
    ("SRC_COLOR", 0x0300 as f64),
    ("ONE_MINUS_SRC_COLOR", 0x0301 as f64),
    ("SRC_ALPHA", 0x0302 as f64),
    ("ONE_MINUS_SRC_ALPHA", 0x0303 as f64),
    ("DST_ALPHA", 0x0304 as f64),
    ("ONE_MINUS_DST_ALPHA", 0x0305 as f64),
    ("DST_COLOR", 0x0306 as f64),
    ("ONE_MINUS_DST_COLOR", 0x0307 as f64),
    ("SRC_ALPHA_SATURATE", 0x0308 as f64),
    ("CONSTANT_COLOR", 0x8001 as f64),
    ("ONE_MINUS_CONSTANT_COLOR", 0x8002 as f64),
    ("CONSTANT_ALPHA", 0x8003 as f64),
    ("ONE_MINUS_CONSTANT_ALPHA", 0x8004 as f64),
    ("BLEND_COLOR", 0x8005 as f64),
    ("FUNC_ADD", 0x8006 as f64),
    ("FUNC_SUBTRACT", 0x800A as f64),
    ("FUNC_REVERSE_SUBTRACT", 0x800B as f64),
    ("BLEND_EQUATION", 0x8009 as f64),
    ("BLEND_EQUATION_RGB", 0x8009 as f64),
    ("BLEND_EQUATION_ALPHA", 0x883D as f64),
    ("BLEND_DST_RGB", 0x80C8 as f64),
    ("BLEND_SRC_RGB", 0x80C9 as f64),
    ("BLEND_DST_ALPHA", 0x80CA as f64),
    ("BLEND_SRC_ALPHA", 0x80CB as f64),
    ("MIN", 0x8007 as f64),
    ("MAX", 0x8008 as f64),
    // Buffer objects
    ("BUFFER_SIZE", 0x8764 as f64),
    ("BUFFER_USAGE", 0x8765 as f64),
    ("ARRAY_BUFFER", 0x8892 as f64),
    ("ELEMENT_ARRAY_BUFFER", 0x8893 as f64),
    ("ARRAY_BUFFER_BINDING", 0x8894 as f64),
    ("ELEMENT_ARRAY_BUFFER_BINDING", 0x8895 as f64),
    ("VERTEX_ATTRIB_ARRAY_BUFFER_BINDING", 0x889F as f64),
    ("STREAM_DRAW", 0x88E0 as f64),
    ("STATIC_DRAW", 0x88E4 as f64),
    ("DYNAMIC_DRAW", 0x88E8 as f64),
    // Culling
    ("CULL_FACE", 0x0B44 as f64),
    ("FRONT", 0x0404 as f64),
    ("BACK", 0x0405 as f64),
    ("FRONT_AND_BACK", 0x0408 as f64),
    ("FRONT_FACE", 0x0B46 as f64),
    ("CULL_FACE_MODE", 0x0B45 as f64),
    ("CW", 0x0900 as f64),
    ("CCW", 0x0901 as f64),
    // Depth functions
    ("NEVER", 0x0200 as f64),
    ("LESS", 0x0201 as f64),
    ("EQUAL", 0x0202 as f64),
    ("LEQUAL", 0x0203 as f64),
    ("GREATER", 0x0204 as f64),
    ("NOTEQUAL", 0x0205 as f64),
    ("GEQUAL", 0x0206 as f64),
    ("ALWAYS", 0x0207 as f64),
    ("DEPTH_TEST", 0x0B71 as f64),
    ("DEPTH_WRITEMASK", 0x0B72 as f64),
    ("DEPTH_CLEAR_VALUE", 0x0B73 as f64),
    ("DEPTH_FUNC", 0x0B74 as f64),
    ("DEPTH_RANGE", 0x0B70 as f64),
    // Caps
    ("BLEND", 0x0BE2 as f64),
    ("DITHER", 0x0BD0 as f64),
    ("SCISSOR_TEST", 0x0C11 as f64),
    ("POLYGON_OFFSET_FILL", 0x8037 as f64),
    ("POLYGON_OFFSET_FACTOR", 0x8038 as f64),
    ("POLYGON_OFFSET_UNITS", 0x2A00 as f64),
    ("SAMPLE_ALPHA_TO_COVERAGE", 0x809E as f64),
    ("SAMPLE_COVERAGE", 0x80A0 as f64),
    ("SAMPLE_BUFFERS", 0x80A8 as f64),
    ("SAMPLES", 0x80A9 as f64),
    ("SAMPLE_COVERAGE_VALUE", 0x80AA as f64),
    ("SAMPLE_COVERAGE_INVERT", 0x80AB as f64),
    ("STENCIL_TEST", 0x0B90 as f64),
    // Errors
    ("NO_ERROR", 0.0),
    ("INVALID_ENUM", 0x0500 as f64),
    ("INVALID_VALUE", 0x0501 as f64),
    ("INVALID_OPERATION", 0x0502 as f64),
    ("INVALID_FRAMEBUFFER_OPERATION", 0x0506 as f64),
    ("OUT_OF_MEMORY", 0x0505 as f64),
    ("CONTEXT_LOST_WEBGL", 0x9242 as f64),
    // Framebuffers & renderbuffers
    ("FRAMEBUFFER", 0x8D40 as f64),
    ("RENDERBUFFER", 0x8D41 as f64),
    ("FRAMEBUFFER_BINDING", 0x8CA6 as f64),
    ("RENDERBUFFER_BINDING", 0x8CA7 as f64),
    ("MAX_RENDERBUFFER_SIZE", 0x84E8 as f64),
    ("RGBA4", 0x8056 as f64),
    ("RGB5_A1", 0x8057 as f64),
    ("RGB565", 0x8D62 as f64),
    ("DEPTH_COMPONENT16", 0x81A5 as f64),
    ("STENCIL_INDEX8", 0x8D48 as f64),
    ("RENDERBUFFER_WIDTH", 0x8D42 as f64),
    ("RENDERBUFFER_HEIGHT", 0x8D43 as f64),
    ("RENDERBUFFER_INTERNAL_FORMAT", 0x8D44 as f64),
    ("RENDERBUFFER_RED_SIZE", 0x8D50 as f64),
    ("RENDERBUFFER_GREEN_SIZE", 0x8D51 as f64),
    ("RENDERBUFFER_BLUE_SIZE", 0x8D52 as f64),
    ("RENDERBUFFER_ALPHA_SIZE", 0x8D53 as f64),
    ("RENDERBUFFER_DEPTH_SIZE", 0x8D54 as f64),
    ("RENDERBUFFER_STENCIL_SIZE", 0x8D55 as f64),
    ("FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE", 0x8CD0 as f64),
    ("FRAMEBUFFER_ATTACHMENT_OBJECT_NAME", 0x8CD1 as f64),
    ("FRAMEBUFFER_ATTACHMENT_TEXTURE_LEVEL", 0x8CD2 as f64),
    (
        "FRAMEBUFFER_ATTACHMENT_TEXTURE_CUBE_MAP_FACE",
        0x8CD3 as f64,
    ),
    ("COLOR_ATTACHMENT0", 0x8CE0 as f64),
    ("DEPTH_ATTACHMENT", 0x8D00 as f64),
    ("STENCIL_ATTACHMENT", 0x8D20 as f64),
    ("NONE", 0.0),
    ("FRAMEBUFFER_COMPLETE", 0x8CD5 as f64),
    ("FRAMEBUFFER_INCOMPLETE_ATTACHMENT", 0x8CD6 as f64),
    ("FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT", 0x8CD7 as f64),
    ("FRAMEBUFFER_INCOMPLETE_DIMENSIONS", 0x8CD9 as f64),
    ("FRAMEBUFFER_UNSUPPORTED", 0x8CDD as f64),
    // Programs & shaders
    ("DELETE_STATUS", 0x8B80 as f64),
    ("COMPILE_STATUS", 0x8B81 as f64),
    ("LINK_STATUS", 0x8B82 as f64),
    ("VALIDATE_STATUS", 0x8B83 as f64),
    ("ATTACHED_SHADERS", 0x8B85 as f64),
    ("ACTIVE_UNIFORMS", 0x8B86 as f64),
    ("ACTIVE_ATTRIBUTES", 0x8B89 as f64),
    ("VERTEX_SHADER", 0x8B31 as f64),
    ("FRAGMENT_SHADER", 0x8B30 as f64),
    ("SHADER_TYPE", 0x8B4F as f64),
    ("MAX_VERTEX_TEXTURE_IMAGE_UNITS", 0x8B4C as f64),
    ("MAX_COMBINED_TEXTURE_IMAGE_UNITS", 0x8B4D as f64),
    ("MAX_TEXTURE_IMAGE_UNITS", 0x8872 as f64),
    ("MAX_VERTEX_ATTRIBS", 0x8869 as f64),
    ("MAX_VERTEX_UNIFORM_VECTORS", 0x8DFB as f64),
    ("MAX_VARYING_VECTORS", 0x8DFC as f64),
    ("MAX_FRAGMENT_UNIFORM_VECTORS", 0x8DFD as f64),
    ("CURRENT_PROGRAM", 0x8B8D as f64),
    ("SHADING_LANGUAGE_VERSION", 0x8B8C as f64),
    // Hints
    ("DONT_CARE", 0x1100 as f64),
    ("FASTEST", 0x1101 as f64),
    ("NICEST", 0x1102 as f64),
    ("GENERATE_MIPMAP_HINT", 0x8192 as f64),
    // Data types
    ("BYTE", 0x1400 as f64),
    ("UNSIGNED_BYTE", 0x1401 as f64),
    ("SHORT", 0x1402 as f64),
    ("UNSIGNED_SHORT", 0x1403 as f64),
    ("INT", 0x1404 as f64),
    ("UNSIGNED_INT", 0x1405 as f64),
    ("FLOAT", 0x1406 as f64),
    // Uniform / attribute value types
    ("FLOAT_VEC2", 0x8B50 as f64),
    ("FLOAT_VEC3", 0x8B51 as f64),
    ("FLOAT_VEC4", 0x8B52 as f64),
    ("INT_VEC2", 0x8B53 as f64),
    ("INT_VEC3", 0x8B54 as f64),
    ("INT_VEC4", 0x8B55 as f64),
    ("BOOL", 0x8B56 as f64),
    ("BOOL_VEC2", 0x8B57 as f64),
    ("BOOL_VEC3", 0x8B58 as f64),
    ("BOOL_VEC4", 0x8B59 as f64),
    ("FLOAT_MAT2", 0x8B5A as f64),
    ("FLOAT_MAT3", 0x8B5B as f64),
    ("FLOAT_MAT4", 0x8B5C as f64),
    ("SAMPLER_2D", 0x8B5E as f64),
    ("SAMPLER_CUBE", 0x8B60 as f64),
    // Pixel formats & types
    ("DEPTH_COMPONENT", 0x1902 as f64),
    ("ALPHA", 0x1906 as f64),
    ("RGB", 0x1907 as f64),
    ("RGBA", 0x1908 as f64),
    ("LUMINANCE", 0x1909 as f64),
    ("LUMINANCE_ALPHA", 0x190A as f64),
    ("UNSIGNED_SHORT_4_4_4_4", 0x8033 as f64),
    ("UNSIGNED_SHORT_5_5_5_1", 0x8034 as f64),
    ("UNSIGNED_SHORT_5_6_5", 0x8036 as f64),
    ("IMPLEMENTATION_COLOR_READ_TYPE", 0x8B3A as f64),
    ("IMPLEMENTATION_COLOR_READ_FORMAT", 0x8B3B as f64),
    // Pixel storage
    ("UNPACK_ALIGNMENT", 0x0CF5 as f64),
    ("PACK_ALIGNMENT", 0x0D05 as f64),
    ("UNPACK_FLIP_Y_WEBGL", 0x9240 as f64),
    ("UNPACK_PREMULTIPLY_ALPHA_WEBGL", 0x9241 as f64),
    ("UNPACK_COLORSPACE_CONVERSION_WEBGL", 0x9243 as f64),
    ("BROWSER_DEFAULT_WEBGL", 0x9244 as f64),
    // Textures
    ("TEXTURE", 0x1702 as f64),
    ("TEXTURE_2D", 0x0DE1 as f64),
    ("TEXTURE_WRAP_S", 0x2802 as f64),
    ("TEXTURE_WRAP_T", 0x2803 as f64),
    ("TEXTURE_MAG_FILTER", 0x2800 as f64),
    ("TEXTURE_MIN_FILTER", 0x2801 as f64),
    ("TEXTURE_CUBE_MAP", 0x8513 as f64),
    ("TEXTURE_BINDING_2D", 0x8069 as f64),
    ("TEXTURE_BINDING_CUBE_MAP", 0x8514 as f64),
    ("TEXTURE_CUBE_MAP_POSITIVE_X", 0x8515 as f64),
    ("TEXTURE_CUBE_MAP_NEGATIVE_X", 0x8516 as f64),
    ("TEXTURE_CUBE_MAP_POSITIVE_Y", 0x8517 as f64),
    ("TEXTURE_CUBE_MAP_NEGATIVE_Y", 0x8518 as f64),
    ("TEXTURE_CUBE_MAP_POSITIVE_Z", 0x8519 as f64),
    ("TEXTURE_CUBE_MAP_NEGATIVE_Z", 0x851A as f64),
    ("REPEAT", 0x2901 as f64),
    ("CLAMP_TO_EDGE", 0x812F as f64),
    ("MIRRORED_REPEAT", 0x8370 as f64),
    ("NEAREST", 0x2600 as f64),
    ("LINEAR", 0x2601 as f64),
    ("NEAREST_MIPMAP_NEAREST", 0x2700 as f64),
    ("LINEAR_MIPMAP_NEAREST", 0x2701 as f64),
    ("NEAREST_MIPMAP_LINEAR", 0x2702 as f64),
    ("LINEAR_MIPMAP_LINEAR", 0x2703 as f64),
    ("MAX_TEXTURE_SIZE", 0x0D33 as f64),
    ("MAX_CUBE_MAP_TEXTURE_SIZE", 0x851C as f64),
    ("ACTIVE_TEXTURE", 0x84E0 as f64),
    // Vertex attribs
    ("VERTEX_ATTRIB_ARRAY_ENABLED", 0x8622 as f64),
    ("VERTEX_ATTRIB_ARRAY_SIZE", 0x8623 as f64),
    ("VERTEX_ATTRIB_ARRAY_STRIDE", 0x8624 as f64),
    ("VERTEX_ATTRIB_ARRAY_TYPE", 0x8625 as f64),
    ("VERTEX_ATTRIB_ARRAY_NORMALIZED", 0x886A as f64),
    ("CURRENT_VERTEX_ATTRIB", 0x8626 as f64),
    // Query boxes and ranges
    ("SCISSOR_BOX", 0x0C10 as f64),
    ("VIEWPORT", 0x0BA2 as f64),
    ("MAX_VIEWPORT_DIMS", 0x0D3A as f64),
    ("SUBPIXEL_BITS", 0x0D50 as f64),
    ("RED_BITS", 0x0D52 as f64),
    ("GREEN_BITS", 0x0D53 as f64),
    ("BLUE_BITS", 0x0D54 as f64),
    ("ALPHA_BITS", 0x0D55 as f64),
    ("DEPTH_BITS", 0x0D56 as f64),
    ("STENCIL_BITS", 0x0D57 as f64),
    ("ALIASED_POINT_SIZE_RANGE", 0x846D as f64),
    ("ALIASED_LINE_WIDTH_RANGE", 0x846E as f64),
    ("COLOR_CLEAR_VALUE", 0x0C22 as f64),
    ("COLOR_WRITEMASK", 0x0C23 as f64),
    ("LINE_WIDTH", 0x0B21 as f64),
    ("COMPRESSED_TEXTURE_FORMATS", 0x86A3 as f64),
    // Identity strings
    ("VENDOR", 0x1F00 as f64),
    ("RENDERER", 0x1F01 as f64),
    ("VERSION", 0x1F02 as f64),
    ("EXTENSIONS", 0x1F03 as f64),
    // Stencil state & ops
    ("STENCIL_FUNC", 0x0B92 as f64),
    ("STENCIL_VALUE_MASK", 0x0B9A as f64),
    ("STENCIL_REF", 0x0B97 as f64),
    ("STENCIL_FAIL", 0x0B94 as f64),
    ("STENCIL_PASS_DEPTH_FAIL", 0x0B95 as f64),
    ("STENCIL_PASS_DEPTH_PASS", 0x0B96 as f64),
    ("STENCIL_BACK_FUNC", 0x8800 as f64),
    ("STENCIL_BACK_FAIL", 0x8801 as f64),
    ("STENCIL_BACK_PASS_DEPTH_FAIL", 0x8802 as f64),
    ("STENCIL_BACK_PASS_DEPTH_PASS", 0x8803 as f64),
    ("STENCIL_BACK_REF", 0x8CA3 as f64),
    ("STENCIL_BACK_VALUE_MASK", 0x8CA4 as f64),
    ("STENCIL_BACK_WRITEMASK", 0x8CA5 as f64),
    ("STENCIL_WRITEMASK", 0x0B98 as f64),
    ("KEEP", 0x1E00 as f64),
    ("REPLACE", 0x1E01 as f64),
    ("INCR", 0x1E02 as f64),
    ("DECR", 0x1E03 as f64),
    ("INVERT", 0x150A as f64),
    ("INCR_WRAP", 0x8507 as f64),
    ("DECR_WRAP", 0x8508 as f64),
    // WEBGL_debug_renderer_info
    ("UNMASKED_VENDOR_WEBGL", 37445.0),
    ("UNMASKED_RENDERER_WEBGL", 37446.0),
];

/// Additional constants for WebGL2 contexts.
const CONSTANTS_WEBGL2: &[(&str, f64)] = &[
    // Texture formats & targets
    ("TEXTURE_3D", 0x806F as f64),
    ("TEXTURE_2D_ARRAY", 0x8C1A as f64),
    ("TEXTURE_BINDING_3D", 0x806A as f64),
    ("TEXTURE_BINDING_2D_ARRAY", 0x8C1D as f64),
    ("TEXTURE_WRAP_R", 0x8072 as f64),
    ("MAX_3D_TEXTURE_SIZE", 0x8073 as f64),
    ("MAX_ARRAY_TEXTURE_LAYERS", 0x88FF as f64),
    ("TEXTURE_IMMUTABLE_FORMAT", 0x912F as f64),
    ("RGBA32F", 0x8814 as f64),
    ("RGBA16F", 0x881A as f64),
    ("RGB32F", 0x8815 as f64),
    ("RGB16F", 0x881B as f64),
    ("RGBA8", 0x8058 as f64),
    ("RGB10_A2", 0x8059 as f64),
    ("SRGB8_ALPHA8", 0x8C43 as f64),
    ("R8", 0x8229 as f64),
    ("RG8", 0x822B as f64),
    ("R16F", 0x822D as f64),
    ("R32F", 0x822E as f64),
    ("RG16F", 0x822F as f64),
    ("RG32F", 0x8230 as f64),
    ("RED", 0x1903 as f64),
    ("RG", 0x8227 as f64),
    ("HALF_FLOAT", 0x140B as f64),
    ("UNSIGNED_INT_10F_11F_11F_REV", 0x8C3B as f64),
    ("UNSIGNED_INT_2_10_10_10_REV", 0x8368 as f64),
    ("UNSIGNED_INT_5_9_9_9_REV", 0x8C3E as f64),
    ("FLOAT_32_UNSIGNED_INT_24_8_REV", 0x8DAD as f64),
    ("DEPTH_COMPONENT24", 0x81A6 as f64),
    ("DEPTH_COMPONENT32F", 0x8CAC as f64),
    ("DEPTH24_STENCIL8", 0x88F0 as f64),
    ("RED_INTEGER", 0x8D94 as f64),
    ("RG_INTEGER", 0x8D95 as f64),
    ("RGB_INTEGER", 0x8D98 as f64),
    ("RGBA_INTEGER", 0x8D99 as f64),
    // Buffers
    ("STREAM_READ", 0x88E1 as f64),
    ("STREAM_COPY", 0x88E2 as f64),
    ("STATIC_READ", 0x88E5 as f64),
    ("STATIC_COPY", 0x88E6 as f64),
    ("DYNAMIC_READ", 0x88E9 as f64),
    ("DYNAMIC_COPY", 0x88EA as f64),
    ("PIXEL_PACK_BUFFER", 0x8EB2 as f64),
    ("PIXEL_UNPACK_BUFFER", 0x8EB3 as f64),
    ("COPY_READ_BUFFER", 0x8F36 as f64),
    ("COPY_WRITE_BUFFER", 0x8F37 as f64),
    ("UNIFORM_BUFFER", 0x8A11 as f64),
    ("UNIFORM_BUFFER_BINDING", 0x8A28 as f64),
    ("VERTEX_ATTRIB_ARRAY_INTEGER", 0x88FD as f64),
    // Framebuffers
    ("READ_FRAMEBUFFER", 0x8CA8 as f64),
    ("DRAW_FRAMEBUFFER", 0x8CA9 as f64),
    ("MAX_COLOR_ATTACHMENTS", 0x8CDF as f64),
    ("MAX_DRAW_BUFFERS", 0x8824 as f64),
    ("MAX_SAMPLES", 0x8D57 as f64),
    ("MAX_ELEMENTS_INDICES", 0x80E9 as f64),
    ("MAX_ELEMENTS_VERTICES", 0x80E8 as f64),
    // Vertex arrays & instancing
    ("VERTEX_ARRAY_BINDING", 0x85B5 as f64),
    // Sync objects
    ("SYNC_FLUSH_COMMANDS_BIT", 1.0),
    ("MAX_SERVER_WAIT_TIMEOUT", 0x9111 as f64),
    ("OBJECT_TYPE", 0x9112 as f64),
    ("SYNC_CONDITION", 0x9113 as f64),
    ("SYNC_STATUS", 0x9114 as f64),
    ("SYNC_FLAGS", 0x9115 as f64),
    ("SYNC_FENCE", 0x9116 as f64),
    ("SYNC_GPU_COMMANDS_COMPLETE", 0x9117 as f64),
    ("UNSIGNALED", 0x9118 as f64),
    ("SIGNALED", 0x9119 as f64),
    ("ALREADY_SIGNALED", 0x911A as f64),
    ("TIMEOUT_EXPIRED", 0x911B as f64),
    ("WAIT_FAILED", 0x911C as f64),
    ("FRAGMENT_SHADER_DERIVATIVE_HINT", 0x8B8B as f64),
];

// ─── Registration macro ─────────────────────────────────────────────────────

/// Registers one context method as an own property on `obj`.
///
/// Body runs inside [`super::with_gl`]: `$entry: &mut RegistryEntry`,
/// `$gl: &glow::Context`, `$args: &[JsValue]`, `$mcx: &mut Context`.
/// A `GlError` (lost/stale context) makes the method return `undefined`.
macro_rules! gl_method {
    ($cx:expr, $obj:expr, $name:literal, move |$handle:ident, $entry:ident, $gl:ident, $args:ident, $mcx:ident| $body:expr) => {{
        // The unsafe context must NOT lexically wrap the closure body
        // (edition 2024 closures inherit it): inner glow calls keep their own
        // explicit `unsafe` blocks this way.
        let closure =
            move |this: &JsValue, args: &[JsValue], cx: &mut Context| -> JsResult<JsValue> {
                let Some($handle) = this_handle(this, cx) else {
                    return Ok(JsValue::undefined());
                };
                let out = super::with_gl(
                    $handle,
                    |entry: &mut RegistryEntry, gl: &glow::Context| -> JsResult<JsValue> {
                        let ($entry, $gl, $args, $mcx) = (entry, gl, args, cx);
                        $body
                    },
                );
                match out {
                    Ok(v) => v,
                    // Lost/stale context: silently no-op (WebGL spec).
                    Err(_) => Ok(JsValue::undefined()),
                }
            };
        let f = unsafe { NativeFunction::from_closure(closure) };
        let func = JsValue::from(
            FunctionObjectBuilder::new($cx.realm(), f)
                .name(js_string!($name))
                .build(),
        );
        let _ = $obj.set(js_string!($name), func, false, $cx);
    }};
}

// ─── Method registration ────────────────────────────────────────────────────

/// Registers the full method surface for `kind` on the context object.
fn register_methods(cx: &mut Context, obj: &JsObject, kind: CtxKind) {
    // ── Resource creation ──
    gl_method!(cx, obj, "createShader", move |h, e, gl, args, c| {
        let ty = u32_at(args, 0)?;
        if ty != glow::VERTEX_SHADER && ty != glow::FRAGMENT_SHADER {
            e.ctx.push_error(types::INVALID_ENUM);
            return Ok(JsValue::undefined());
        }
        match unsafe { gl.create_shader(ty) } {
            Ok(s) => Ok(new_resource(c, e, h, ResourceKind::Shader, s.0.get())),
            Err(_) => {
                e.ctx.push_error(types::OUT_OF_MEMORY);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(cx, obj, "createProgram", move |h, e, gl, _args, c| {
        match unsafe { gl.create_program() } {
            Ok(p) => Ok(new_resource(c, e, h, ResourceKind::Program, p.0.get())),
            Err(_) => {
                e.ctx.push_error(types::OUT_OF_MEMORY);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(cx, obj, "createBuffer", move |h, e, gl, _args, c| {
        match unsafe { gl.create_buffer() } {
            Ok(b) => Ok(new_resource(c, e, h, ResourceKind::Buffer, b.0.get())),
            Err(_) => {
                e.ctx.push_error(types::OUT_OF_MEMORY);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(cx, obj, "createTexture", move |h, e, gl, _args, c| {
        match unsafe { gl.create_texture() } {
            Ok(t) => Ok(new_resource(c, e, h, ResourceKind::Texture, t.0.get())),
            Err(_) => {
                e.ctx.push_error(types::OUT_OF_MEMORY);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(cx, obj, "createFramebuffer", move |h, e, gl, _args, c| {
        match unsafe { gl.create_framebuffer() } {
            Ok(f) => Ok(new_resource(c, e, h, ResourceKind::Framebuffer, f.0.get())),
            Err(_) => {
                e.ctx.push_error(types::OUT_OF_MEMORY);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(cx, obj, "createRenderbuffer", move |h, e, gl, _args, c| {
        match unsafe { gl.create_renderbuffer() } {
            Ok(r) => Ok(new_resource(c, e, h, ResourceKind::Renderbuffer, r.0.get())),
            Err(_) => {
                e.ctx.push_error(types::OUT_OF_MEMORY);
                Ok(JsValue::undefined())
            }
        }
    });

    // ── Shader methods ──
    gl_method!(cx, obj, "shaderSource", move |h, e, gl, args, c| {
        let src = str_at(args, 1)?;
        if let Some(sh) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Shader)?
        {
            unsafe { gl.shader_source(glow::NativeShader(nz(sh)), &src) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "getShaderInfoLog", move |h, e, gl, args, c| {
        if let Some(sh) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Shader)?
        {
            let log = unsafe { gl.get_shader_info_log(glow::NativeShader(nz(sh))) };
            return Ok(JsValue::from(JsString::from(log)));
        }
        Ok(JsValue::from(JsString::from("")))
    });
    gl_method!(cx, obj, "compileShader", move |h, e, gl, args, c| {
        if let Some(sh) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Shader)?
        {
            unsafe { gl.compile_shader(glow::NativeShader(nz(sh))) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "getShaderParameter", move |h, e, gl, args, c| {
        let pname = u32_at(args, 1)?;
        let Some(sh) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Shader)?
        else {
            return Ok(JsValue::undefined());
        };
        let native = glow::NativeShader(nz(sh));
        match pname {
            // COMPILE_STATUS
            0x8B81 => Ok(JsValue::from(unsafe {
                gl.get_shader_compile_status(native)
            })),
            // DELETE_STATUS — mirrored in frontend resource state.
            0x8B80 => Ok(JsValue::from(
                unwrap_wrapper(args.get_or_undefined(0), c)
                    .and_then(|(_, id, _)| e.ctx.resource(id))
                    .map(|r| r.deleted)
                    .unwrap_or(true),
            )),
            // SHADER_TYPE: glow 0.16 exposes no glGetShaderiv for it; report
            // INVALID_ENUM rather than guess.
            _ => {
                e.ctx.push_error(types::INVALID_ENUM);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(
        cx,
        obj,
        "getShaderPrecisionFormat",
        move |h, e, gl, args, c| {
            let stype = u32_at(args, 0)?;
            let ptype = u32_at(args, 1)?;
            match unsafe { gl.get_shader_precision_format(stype, ptype) } {
                Some(fmt) => {
                    let o = JsObject::with_object_proto(c.intrinsics());
                    let _ = o.set(
                        js_string!("rangeMin"),
                        JsValue::from(fmt.range_min as f64),
                        false,
                        c,
                    );
                    let _ = o.set(
                        js_string!("rangeMax"),
                        JsValue::from(fmt.range_max as f64),
                        false,
                        c,
                    );
                    let _ = o.set(
                        js_string!("precision"),
                        JsValue::from(fmt.precision as f64),
                        false,
                        c,
                    );
                    Ok(JsValue::from(o))
                }
                None => {
                    e.ctx.push_error(types::INVALID_ENUM);
                    Ok(JsValue::undefined())
                }
            }
        }
    );

    // ── Program methods ──
    gl_method!(cx, obj, "attachShader", move |h, e, gl, args, c| {
        if let (Some(p), Some(s)) = (
            resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?,
            resolve_resource(e, h, args.get_or_undefined(1), c, ResourceKind::Shader)?,
        ) {
            unsafe { gl.attach_shader(glow::NativeProgram(nz(p)), glow::NativeShader(nz(s))) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "detachShader", move |h, e, gl, args, c| {
        if let (Some(p), Some(s)) = (
            resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?,
            resolve_resource(e, h, args.get_or_undefined(1), c, ResourceKind::Shader)?,
        ) {
            unsafe { gl.detach_shader(glow::NativeProgram(nz(p)), glow::NativeShader(nz(s))) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "linkProgram", move |h, e, gl, args, c| {
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        {
            unsafe { gl.link_program(glow::NativeProgram(nz(p))) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "validateProgram", move |h, e, gl, args, c| {
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        {
            unsafe { gl.validate_program(glow::NativeProgram(nz(p))) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "useProgram", move |h, e, gl, args, c| {
        let v = args.get_or_undefined(0);
        if v.is_null_or_undefined() {
            unsafe { gl.use_program(None) };
            e.ctx.used_program = None;
            return Ok(JsValue::undefined());
        }
        if let Some(p) = resolve_resource(e, h, v, c, ResourceKind::Program)? {
            let id = unwrap_wrapper(v, c).map(|x| x.1);
            unsafe { gl.use_program(Some(glow::NativeProgram(nz(p)))) };
            e.ctx.used_program = id;
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "bindAttribLocation", move |h, e, gl, args, c| {
        let index = u32_at(args, 1)?;
        let name = str_at(args, 2)?;
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        {
            unsafe { gl.bind_attrib_location(glow::NativeProgram(nz(p)), index, &name) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "getAttribLocation", move |h, e, gl, args, c| {
        let name = str_at(args, 1)?;
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        {
            return match unsafe { gl.get_attrib_location(glow::NativeProgram(nz(p)), &name) } {
                Some(loc) => Ok(JsValue::from(loc as i32 as f64)),
                None => Ok(JsValue::from(-1.0)),
            };
        }
        Ok(JsValue::from(-1.0))
    });
    gl_method!(cx, obj, "getUniformLocation", move |h, e, gl, args, c| {
        let name = str_at(args, 1)?;
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        {
            // glow's NativeUniformLocation wraps a plain GLuint (location 0 is
            // valid), so the raw value is stored as the resource handle.
            if let Some(loc) = unsafe { gl.get_uniform_location(glow::NativeProgram(nz(p)), &name) }
            {
                return Ok(new_resource(c, e, h, ResourceKind::UniformLocation, loc.0));
            }
            return Ok(JsValue::null());
        }
        Ok(JsValue::null())
    });
    gl_method!(cx, obj, "getProgramInfoLog", move |h, e, gl, args, c| {
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        {
            let log = unsafe { gl.get_program_info_log(glow::NativeProgram(nz(p))) };
            return Ok(JsValue::from(JsString::from(log)));
        }
        Ok(JsValue::from(JsString::from("")))
    });
    gl_method!(cx, obj, "getProgramParameter", move |h, e, gl, args, c| {
        let pname = u32_at(args, 1)?;
        let v = args.get_or_undefined(0);
        let Some(p) = resolve_resource(e, h, v, c, ResourceKind::Program)? else {
            return Ok(JsValue::undefined());
        };
        let native = glow::NativeProgram(nz(p));
        match pname {
            // LINK_STATUS
            0x8B82 => Ok(JsValue::from(unsafe { gl.get_program_link_status(native) })),
            // VALIDATE_STATUS
            0x8B83 => Ok(JsValue::from(unsafe {
                gl.get_program_validate_status(native)
            })),
            // ACTIVE_ATTRIBUTES / ACTIVE_UNIFORMS
            0x8B89 => Ok(JsValue::from(
                unsafe { gl.get_active_attributes(native) } as f64
            )),
            0x8B86 => Ok(JsValue::from(
                unsafe { gl.get_active_uniforms(native) } as f64
            )),
            // ATTACHED_SHADERS
            0x8B85 => Ok(JsValue::from(
                unsafe { gl.get_program_parameter_i32(native, pname) } as f64,
            )),
            // DELETE_STATUS — frontend mirror.
            0x8B80 => Ok(JsValue::from(
                unwrap_wrapper(v, c)
                    .and_then(|(_, id, _)| e.ctx.resource(id))
                    .map(|r| r.deleted)
                    .unwrap_or(true),
            )),
            _ => {
                e.ctx.push_error(types::INVALID_ENUM);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(cx, obj, "getActiveUniform", move |h, e, gl, args, c| {
        let index = u32_at(args, 1)?;
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
            && let Some(u) = unsafe { gl.get_active_uniform(glow::NativeProgram(nz(p)), index) }
        {
            let o = JsObject::with_object_proto(c.intrinsics());
            let _ = o.set(js_string!("size"), JsValue::from(u.size as f64), false, c);
            let _ = o.set(js_string!("type"), JsValue::from(u.utype as f64), false, c);
            let _ = o.set(
                js_string!("name"),
                JsValue::from(JsString::from(u.name)),
                false,
                c,
            );
            return Ok(JsValue::from(o));
        }
        e.ctx.push_error(types::INVALID_VALUE);
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "getActiveAttrib", move |h, e, gl, args, c| {
        let index = u32_at(args, 1)?;
        if let Some(p) = resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
            && let Some(a) = unsafe { gl.get_active_attribute(glow::NativeProgram(nz(p)), index) }
        {
            let o = JsObject::with_object_proto(c.intrinsics());
            let _ = o.set(js_string!("size"), JsValue::from(a.size as f64), false, c);
            let _ = o.set(js_string!("type"), JsValue::from(a.atype as f64), false, c);
            let _ = o.set(
                js_string!("name"),
                JsValue::from(JsString::from(a.name)),
                false,
                c,
            );
            return Ok(JsValue::from(o));
        }
        e.ctx.push_error(types::INVALID_VALUE);
        Ok(JsValue::undefined())
    });

    // ── Buffers ──
    gl_method!(cx, obj, "bindBuffer", move |h, e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let v = args.get_or_undefined(1);
        let native = if v.is_null_or_undefined() {
            e.ctx.bound_array_buffer = None;
            e.ctx.bound_element_array_buffer = None;
            None
        } else {
            let Some(b) = resolve_resource(e, h, v, c, ResourceKind::Buffer)? else {
                return Ok(JsValue::undefined());
            };
            let id = unwrap_wrapper(v, c).map(|x| x.1);
            match target {
                glow::ARRAY_BUFFER => e.ctx.bound_array_buffer = id,
                glow::ELEMENT_ARRAY_BUFFER => e.ctx.bound_element_array_buffer = id,
                // Unknown targets: forwarded; ANGLE generates GL_INVALID_ENUM.
                _ => {}
            }
            Some(glow::NativeBuffer(nz(b)))
        };
        unsafe { gl.bind_buffer(target, native) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "bufferData", move |_h, _e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let usage = u32_at(args, 2)?;
        let src = args.get_or_undefined(1);
        if let Some(size) = src.as_number() {
            unsafe { gl.buffer_data_size(target, size as i32, usage) };
            return Ok(JsValue::undefined());
        }
        match buffer_source_bytes(src, c)? {
            Some(bytes) => unsafe { gl.buffer_data_u8_slice(target, &bytes, usage) },
            None => return Err(type_err("bufferData requires a size or a buffer source")),
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "bufferSubData", move |_h, _e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let offset = i32_at(args, 1)?;
        let Some(bytes) = buffer_source_bytes(args.get_or_undefined(2), c)? else {
            return Err(type_err("bufferSubData requires a buffer source"));
        };
        unsafe { gl.buffer_sub_data_u8_slice(target, offset, &bytes) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "getBufferParameter", move |_h, e, gl, args, _c| {
        let target = u32_at(args, 0)?;
        let pname = u32_at(args, 1)?;
        match pname {
            // BUFFER_SIZE / BUFFER_USAGE
            0x8764 | 0x8765 => {
                Ok(JsValue::from(
                    unsafe { gl.get_buffer_parameter_i32(target, pname) } as f64,
                ))
            }
            _ => {
                e.ctx.push_error(types::INVALID_ENUM);
                Ok(JsValue::undefined())
            }
        }
    });

    // ── Textures ──
    gl_method!(cx, obj, "bindTexture", move |h, e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let v = args.get_or_undefined(1);
        let native = if v.is_null_or_undefined() {
            match target {
                glow::TEXTURE_2D => e.ctx.bound_texture_2d = None,
                glow::TEXTURE_CUBE_MAP => e.ctx.bound_texture_cube_map = None,
                _ => {}
            }
            None
        } else {
            let Some(t) = resolve_resource(e, h, v, c, ResourceKind::Texture)? else {
                return Ok(JsValue::undefined());
            };
            let id = unwrap_wrapper(v, c).map(|x| x.1);
            match target {
                glow::TEXTURE_2D => e.ctx.bound_texture_2d = id,
                glow::TEXTURE_CUBE_MAP => e.ctx.bound_texture_cube_map = id,
                _ => {}
            }
            Some(glow::NativeTexture(nz(t)))
        };
        unsafe { gl.bind_texture(target, native) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "texParameteri", move |_h, _e, gl, args, _c| {
        let target = u32_at(args, 0)?;
        let pname = u32_at(args, 1)?;
        let param = i32_at(args, 2)?;
        unsafe { gl.tex_parameter_i32(target, pname, param) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "texParameterf", move |_h, _e, gl, args, _c| {
        let target = u32_at(args, 0)?;
        let pname = u32_at(args, 1)?;
        let param = f32_at(args, 2)?;
        unsafe { gl.tex_parameter_f32(target, pname, param) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "getTexParameter", move |_h, _e, gl, args, _c| {
        let target = u32_at(args, 0)?;
        let pname = u32_at(args, 1)?;
        // ANGLE validates the target/pname pairing.
        Ok(JsValue::from(
            unsafe { gl.get_tex_parameter_i32(target, pname) } as f64,
        ))
    });
    gl_method!(cx, obj, "activeTexture", move |_h, e, gl, args, _c| {
        let unit = u32_at(args, 0)?;
        // WebGL guarantees at least 8 units; mirror anything in a 32-unit
        // range and let ANGLE reject beyond what the implementation supports.
        if !(glow::TEXTURE0..glow::TEXTURE0 + 32).contains(&unit) {
            e.ctx.push_error(types::INVALID_ENUM);
            return Ok(JsValue::undefined());
        }
        e.ctx.active_texture_unit = unit - glow::TEXTURE0;
        unsafe { gl.active_texture(unit) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "pixelStorei", move |_h, e, gl, args, _c| {
        let pname = u32_at(args, 0)?;
        let param = i32_at(args, 1)?;
        match pname {
            // WebGL-only knobs with no GLES enum: mirror on the frontend and
            // apply as a CPU transform in the texImage2D path.
            0x9240 => e.ctx.unpack_flip_y = param != 0,
            0x9241 => e.ctx.unpack_premultiply = param != 0,
            // Colorspace conversion is not applied on the CPU path; accept and
            // ignore (uploads behave as BROWSER_DEFAULT_WEBGL).
            0x9243 => {}
            _ => unsafe { gl.pixel_store_i32(pname, param) },
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "generateMipmap", move |_h, _e, gl, args, _c| {
        let target = u32_at(args, 0)?;
        unsafe { gl.generate_mipmap(target) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "texImage2D", move |_h, e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let level = i32_at(args, 1)?;
        let internal_format = i32_at(args, 2)?;
        // Dimensions per overload; pixels extracted exactly once. `owned` must
        // outlive the upload borrow handed to glow.
        let (width, height, border, format, ty, mut owned) = if args.len() >= 9 {
            let w = i32_at(args, 3)?;
            let h2 = i32_at(args, 4)?;
            let border = i32_at(args, 5)?;
            let f = u32_at(args, 6)?;
            let t = u32_at(args, 7)?;
            let px = buffer_source_bytes(args.get_or_undefined(8), c)?;
            (w, h2, border, f, t, px)
        } else if args.len() >= 6 {
            // Legacy 6-arg overload. ArrayBufferView is not a TexImageSource
            // in the spec; we accept it with a square-dimension convention
            // ([`square_dims`]) and let ANGLE validate the resulting upload.
            let f = u32_at(args, 3)?;
            let t = u32_at(args, 4)?;
            let Some(bytes) = buffer_source_bytes(args.get_or_undefined(5), c)? else {
                return Err(type_err("texImage2D requires pixel data"));
            };
            let Some(side) = square_dims(&bytes, e, f, t) else {
                return Ok(JsValue::undefined());
            };
            (side, side, 0, f, t, Some(bytes))
        } else {
            return Err(type_err("texImage2D expects 6 or 9 arguments"));
        };
        if let Some(b) = owned.as_mut() {
            apply_unpack_transform(
                b,
                width,
                height,
                format,
                ty,
                e.ctx.unpack_flip_y,
                e.ctx.unpack_premultiply,
            );
        }
        let upload = match owned.as_deref() {
            None => glow::PixelUnpackData::Slice(None),
            Some(slice) => glow::PixelUnpackData::Slice(Some(slice)),
        };
        unsafe {
            gl.tex_image_2d(
                target,
                level,
                internal_format,
                width,
                height,
                border,
                format,
                ty,
                upload,
            )
        };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "texSubImage2D", move |_h, e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let level = i32_at(args, 1)?;
        let (xoff, yoff, width, height, format, ty, bytes) = if args.len() >= 9 {
            let x = i32_at(args, 2)?;
            let y = i32_at(args, 3)?;
            let w = i32_at(args, 4)?;
            let h2 = i32_at(args, 5)?;
            let f = u32_at(args, 6)?;
            let t = u32_at(args, 7)?;
            let px = buffer_source_bytes(args.get_or_undefined(8), c)?;
            (x, y, w, h2, f, t, px)
        } else if args.len() >= 7 {
            // 7-arg legacy form: dimensions derived from the data length
            // (square convention, mirrors the 6-arg texImage2D handling).
            let x = i32_at(args, 2)?;
            let y = i32_at(args, 3)?;
            let f = u32_at(args, 4)?;
            let t = u32_at(args, 5)?;
            let Some(bytes) = buffer_source_bytes(args.get_or_undefined(6), c)? else {
                // glTexSubImage2D needs real client data; null → INVALID_VALUE.
                e.ctx.push_error(types::INVALID_VALUE);
                return Ok(JsValue::undefined());
            };
            let Some(side) = square_dims(&bytes, e, f, t) else {
                return Ok(JsValue::undefined());
            };
            (x, y, side, side, f, t, Some(bytes))
        } else {
            return Err(type_err("texSubImage2D expects 7 or 9 arguments"));
        };
        let Some(mut bytes) = bytes else {
            e.ctx.push_error(types::INVALID_VALUE);
            return Ok(JsValue::undefined());
        };
        apply_unpack_transform(
            &mut bytes,
            width,
            height,
            format,
            ty,
            e.ctx.unpack_flip_y,
            e.ctx.unpack_premultiply,
        );
        unsafe {
            gl.tex_sub_image_2d(
                target,
                level,
                xoff,
                yoff,
                width,
                height,
                format,
                ty,
                glow::PixelUnpackData::Slice(Some(&bytes)),
            )
        };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "copyTexImage2D", move |_h, _e, gl, args, _c| {
        let (target, level, internal_format) =
            (u32_at(args, 0)?, i32_at(args, 1)?, u32_at(args, 2)?);
        let (x, y, w, h2, border) = (
            i32_at(args, 3)?,
            i32_at(args, 4)?,
            i32_at(args, 5)?,
            i32_at(args, 6)?,
            i32_at(args, 7)?,
        );
        unsafe { gl.copy_tex_image_2d(target, level, internal_format, x, y, w, h2, border) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "copyTexSubImage2D", move |_h, _e, gl, args, _c| {
        let (target, level) = (u32_at(args, 0)?, i32_at(args, 1)?);
        let (xoff, yoff, x, y, w, h2) = (
            i32_at(args, 2)?,
            i32_at(args, 3)?,
            i32_at(args, 4)?,
            i32_at(args, 5)?,
            i32_at(args, 6)?,
            i32_at(args, 7)?,
        );
        unsafe { gl.copy_tex_sub_image_2d(target, level, xoff, yoff, x, y, w, h2) };
        Ok(JsValue::undefined())
    });

    // ── Framebuffers & renderbuffers ──
    gl_method!(cx, obj, "bindFramebuffer", move |h, e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let v = args.get_or_undefined(1);
        let native = if v.is_null_or_undefined() {
            // WebGL1 only knows FRAMEBUFFER; null → no framebuffer bound.
            e.ctx.bound_framebuffer = None;
            None
        } else {
            let Some(f) = resolve_resource(e, h, v, c, ResourceKind::Framebuffer)? else {
                return Ok(JsValue::undefined());
            };
            e.ctx.bound_framebuffer = unwrap_wrapper(v, c).map(|x| x.1);
            Some(glow::NativeFramebuffer(nz(f)))
        };
        unsafe { gl.bind_framebuffer(target, native) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "framebufferTexture2D", move |h, e, gl, args, c| {
        let (target, attachment, textarget, level) = (
            u32_at(args, 0)?,
            u32_at(args, 1)?,
            u32_at(args, 2)?,
            i32_at(args, 4)?,
        );
        let v = args.get_or_undefined(3);
        let native = if v.is_null_or_undefined() {
            None
        } else {
            resolve_resource(e, h, v, c, ResourceKind::Texture)?.map(|t| glow::NativeTexture(nz(t)))
        };
        unsafe { gl.framebuffer_texture_2d(target, attachment, textarget, native, level) };
        Ok(JsValue::undefined())
    });
    gl_method!(
        cx,
        obj,
        "framebufferRenderbuffer",
        move |h, e, gl, args, c| {
            let (target, attachment, rb_target) =
                (u32_at(args, 0)?, u32_at(args, 1)?, u32_at(args, 2)?);
            let v = args.get_or_undefined(3);
            let native = if v.is_null_or_undefined() {
                None
            } else {
                resolve_resource(e, h, v, c, ResourceKind::Renderbuffer)?
                    .map(|r| glow::NativeRenderbuffer(nz(r)))
            };
            unsafe { gl.framebuffer_renderbuffer(target, attachment, rb_target, native) };
            Ok(JsValue::undefined())
        }
    );
    gl_method!(
        cx,
        obj,
        "checkFramebufferStatus",
        move |_h, _e, gl, args, _c| {
            let target = u32_at(args, 0)?;
            Ok(JsValue::from(
                unsafe { gl.check_framebuffer_status(target) } as f64,
            ))
        }
    );
    gl_method!(
        cx,
        obj,
        "getFramebufferAttachmentParameter",
        move |_h, e, gl, args, _c| {
            let (target, attachment, pname) =
                (u32_at(args, 0)?, u32_at(args, 1)?, u32_at(args, 2)?);
            match pname {
                0x8CD0..=0x8CD3 => Ok(JsValue::from(unsafe {
                    gl.get_framebuffer_attachment_parameter_i32(target, attachment, pname)
                } as f64)),
                _ => {
                    e.ctx.push_error(types::INVALID_ENUM);
                    Ok(JsValue::undefined())
                }
            }
        }
    );
    gl_method!(cx, obj, "bindRenderbuffer", move |h, e, gl, args, c| {
        let target = u32_at(args, 0)?;
        let v = args.get_or_undefined(1);
        let native = if v.is_null_or_undefined() {
            e.ctx.bound_renderbuffer = None;
            None
        } else {
            let Some(r) = resolve_resource(e, h, v, c, ResourceKind::Renderbuffer)? else {
                return Ok(JsValue::undefined());
            };
            e.ctx.bound_renderbuffer = unwrap_wrapper(v, c).map(|x| x.1);
            Some(glow::NativeRenderbuffer(nz(r)))
        };
        unsafe { gl.bind_renderbuffer(target, native) };
        Ok(JsValue::undefined())
    });
    gl_method!(
        cx,
        obj,
        "renderbufferStorage",
        move |_h, _e, gl, args, _c| {
            let (target, internal_format, w, h2) = (
                u32_at(args, 0)?,
                u32_at(args, 1)?,
                i32_at(args, 2)?,
                i32_at(args, 3)?,
            );
            unsafe { gl.renderbuffer_storage(target, internal_format, w, h2) };
            Ok(JsValue::undefined())
        }
    );
    gl_method!(
        cx,
        obj,
        "getRenderbufferParameter",
        move |_h, e, gl, args, _c| {
            let (target, pname) = (u32_at(args, 0)?, u32_at(args, 1)?);
            match pname {
                0x8D42 | 0x8D43 | 0x8D44 | 0x8D50 | 0x8D51 | 0x8D52 | 0x8D53 | 0x8D54 | 0x8D55 => {
                    Ok(JsValue::from(
                        unsafe { gl.get_renderbuffer_parameter_i32(target, pname) } as f64,
                    ))
                }
                _ => {
                    e.ctx.push_error(types::INVALID_ENUM);
                    Ok(JsValue::undefined())
                }
            }
        }
    );

    // ── Fixed-function state ──
    gl_method!(cx, obj, "enable", move |_h, _e, gl, args, _c| {
        unsafe { gl.enable(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "disable", move |_h, _e, gl, args, _c| {
        unsafe { gl.disable(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "isEnabled", move |_h, _e, gl, args, _c| {
        Ok(JsValue::from(unsafe { gl.is_enabled(u32_at(args, 0)?) }))
    });
    gl_method!(cx, obj, "viewport", move |_h, _e, gl, args, _c| {
        let (x, y, w, h2) = (
            i32_at(args, 0)?,
            i32_at(args, 1)?,
            i32_at(args, 2)?,
            i32_at(args, 3)?,
        );
        unsafe { gl.viewport(x, y, w, h2) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "clearColor", move |_h, _e, gl, args, _c| {
        let (r, g, b, a) = (
            f32_at(args, 0)?,
            f32_at(args, 1)?,
            f32_at(args, 2)?,
            f32_at(args, 3)?,
        );
        unsafe { gl.clear_color(r, g, b, a) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "clear", move |_h, _e, gl, args, _c| {
        unsafe { gl.clear(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "scissor", move |_h, _e, gl, args, _c| {
        let (x, y, w, h2) = (
            i32_at(args, 0)?,
            i32_at(args, 1)?,
            i32_at(args, 2)?,
            i32_at(args, 3)?,
        );
        unsafe { gl.scissor(x, y, w, h2) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "colorMask", move |_h, _e, gl, args, _c| {
        let (r, g, b, a) = (
            args.get_or_undefined(0).to_boolean(),
            args.get_or_undefined(1).to_boolean(),
            args.get_or_undefined(2).to_boolean(),
            args.get_or_undefined(3).to_boolean(),
        );
        unsafe { gl.color_mask(r, g, b, a) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "depthMask", move |_h, _e, gl, args, _c| {
        unsafe { gl.depth_mask(args.get_or_undefined(0).to_boolean()) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "depthFunc", move |_h, _e, gl, args, _c| {
        unsafe { gl.depth_func(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "clearDepth", move |_h, _e, gl, args, _c| {
        unsafe { gl.clear_depth_f32(f32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "clearStencil", move |_h, _e, gl, args, _c| {
        unsafe { gl.clear_stencil(i32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "frontFace", move |_h, _e, gl, args, _c| {
        unsafe { gl.front_face(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "cullFace", move |_h, _e, gl, args, _c| {
        unsafe { gl.cull_face(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "blendFunc", move |_h, _e, gl, args, _c| {
        let (s, d) = (u32_at(args, 0)?, u32_at(args, 1)?);
        unsafe { gl.blend_func(s, d) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "blendFuncSeparate", move |_h, _e, gl, args, _c| {
        let (sr, dr, sa, da) = (
            u32_at(args, 0)?,
            u32_at(args, 1)?,
            u32_at(args, 2)?,
            u32_at(args, 3)?,
        );
        unsafe { gl.blend_func_separate(sr, dr, sa, da) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "blendColor", move |_h, _e, gl, args, _c| {
        let (r, g, b, a) = (
            f32_at(args, 0)?,
            f32_at(args, 1)?,
            f32_at(args, 2)?,
            f32_at(args, 3)?,
        );
        unsafe { gl.blend_color(r, g, b, a) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "blendEquation", move |_h, _e, gl, args, _c| {
        unsafe { gl.blend_equation(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(
        cx,
        obj,
        "blendEquationSeparate",
        move |_h, _e, gl, args, _c| {
            let (rgb, a) = (u32_at(args, 0)?, u32_at(args, 1)?);
            unsafe { gl.blend_equation_separate(rgb, a) };
            Ok(JsValue::undefined())
        }
    );
    gl_method!(cx, obj, "lineWidth", move |_h, _e, gl, args, _c| {
        unsafe { gl.line_width(f32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "hint", move |_h, _e, gl, args, _c| {
        let (target, mode) = (u32_at(args, 0)?, u32_at(args, 1)?);
        unsafe { gl.hint(target, mode) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "polygonOffset", move |_h, _e, gl, args, _c| {
        let (factor, units) = (f32_at(args, 0)?, f32_at(args, 1)?);
        unsafe { gl.polygon_offset(factor, units) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "sampleCoverage", move |_h, _e, gl, args, _c| {
        let value = f32_at(args, 0)?;
        let invert = args.get_or_undefined(1).to_boolean();
        unsafe { gl.sample_coverage(value, invert) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "stencilFunc", move |_h, _e, gl, args, _c| {
        let (func, reference, mask) = (u32_at(args, 0)?, i32_at(args, 1)?, u32_at(args, 2)?);
        unsafe { gl.stencil_func(func, reference, mask) };
        Ok(JsValue::undefined())
    });
    gl_method!(
        cx,
        obj,
        "stencilFuncSeparate",
        move |_h, _e, gl, args, _c| {
            let (face, func, reference, mask) = (
                u32_at(args, 0)?,
                u32_at(args, 1)?,
                i32_at(args, 2)?,
                u32_at(args, 3)?,
            );
            unsafe { gl.stencil_func_separate(face, func, reference, mask) };
            Ok(JsValue::undefined())
        }
    );
    gl_method!(cx, obj, "stencilMask", move |_h, _e, gl, args, _c| {
        unsafe { gl.stencil_mask(u32_at(args, 0)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(
        cx,
        obj,
        "stencilMaskSeparate",
        move |_h, _e, gl, args, _c| {
            let (face, mask) = (u32_at(args, 0)?, u32_at(args, 1)?);
            unsafe { gl.stencil_mask_separate(face, mask) };
            Ok(JsValue::undefined())
        }
    );
    gl_method!(cx, obj, "stencilOp", move |_h, _e, gl, args, _c| {
        let (sfail, dpfail, dppass) = (u32_at(args, 0)?, u32_at(args, 1)?, u32_at(args, 2)?);
        unsafe { gl.stencil_op(sfail, dpfail, dppass) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "stencilOpSeparate", move |_h, _e, gl, args, _c| {
        let (face, sfail, dpfail, dppass) = (
            u32_at(args, 0)?,
            u32_at(args, 1)?,
            u32_at(args, 2)?,
            u32_at(args, 3)?,
        );
        unsafe { gl.stencil_op_separate(face, sfail, dpfail, dppass) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "finish", move |_h, _e, gl, _args, _c| {
        unsafe { gl.finish() };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "flush", move |_h, _e, gl, _args, _c| {
        unsafe { gl.flush() };
        Ok(JsValue::undefined())
    });

    // ── Vertex attributes ──
    gl_method!(
        cx,
        obj,
        "enableVertexAttribArray",
        move |_h, _e, gl, args, _c| {
            unsafe { gl.enable_vertex_attrib_array(u32_at(args, 0)?) };
            Ok(JsValue::undefined())
        }
    );
    gl_method!(
        cx,
        obj,
        "disableVertexAttribArray",
        move |_h, _e, gl, args, _c| {
            unsafe { gl.disable_vertex_attrib_array(u32_at(args, 0)?) };
            Ok(JsValue::undefined())
        }
    );
    gl_method!(
        cx,
        obj,
        "vertexAttribPointer",
        move |_h, e, gl, args, _c| {
            let (index, size, ty, normalized, stride, offset) = (
                u32_at(args, 0)?,
                i32_at(args, 1)?,
                u32_at(args, 2)?,
                args.get_or_undefined(3).to_boolean(),
                i32_at(args, 4)?,
                i32_at(args, 5)?,
            );
            match ty {
                // BYTE, UNSIGNED_BYTE, SHORT, UNSIGNED_SHORT, FLOAT
                0x1400 | 0x1401 | 0x1402 | 0x1403 | 0x1406 => unsafe {
                    gl.vertex_attrib_pointer_f32(index, size, ty, normalized, stride, offset)
                },
                // INT / UNSIGNED_INT: WebGL2 integer attribute pointers.
                0x1404 | 0x1405 if kind == CtxKind::WebGl2 => unsafe {
                    gl.vertex_attrib_pointer_i32(index, size, ty, stride, offset)
                },
                // HALF_FLOAT / packed types ride the float entry point (WebGL2).
                0x140B | 0x8368 | 0x8C3B if kind == CtxKind::WebGl2 => unsafe {
                    gl.vertex_attrib_pointer_f32(index, size, ty, normalized, stride, offset)
                },
                _ => e.ctx.push_error(types::INVALID_ENUM),
            }
            Ok(JsValue::undefined())
        }
    );
    gl_method!(cx, obj, "getVertexAttrib", move |_h, e, gl, args, c| {
        let (index, pname) = (u32_at(args, 0)?, u32_at(args, 1)?);
        match pname {
            // CURRENT_VERTEX_ATTRIB
            0x8626 => {
                let mut v = [0.0f32; 4];
                unsafe { gl.get_vertex_attrib_parameter_f32_slice(index, pname, &mut v) };
                let arr = JsFloat32Array::from_iter(v.to_vec(), c)?;
                Ok(JsValue::from(arr))
            }
            // VERTEX_ATTRIB_ARRAY_ENABLED / VERTEX_ATTRIB_ARRAY_NORMALIZED
            0x8622 | 0x886A => {
                let mut v = [0.0f32; 1];
                unsafe { gl.get_vertex_attrib_parameter_f32_slice(index, pname, &mut v) };
                Ok(JsValue::from(v[0] != 0.0))
            }
            _ => {
                e.ctx.push_error(types::INVALID_ENUM);
                Ok(JsValue::undefined())
            }
        }
    });
    gl_method!(cx, obj, "vertexAttrib1f", move |_h, _e, gl, args, _c| {
        unsafe { gl.vertex_attrib_1_f32(u32_at(args, 0)?, f32_at(args, 1)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "vertexAttrib2f", move |_h, _e, gl, args, _c| {
        unsafe { gl.vertex_attrib_2_f32(u32_at(args, 0)?, f32_at(args, 1)?, f32_at(args, 2)?) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "vertexAttrib3f", move |_h, _e, gl, args, _c| {
        unsafe {
            gl.vertex_attrib_3_f32(
                u32_at(args, 0)?,
                f32_at(args, 1)?,
                f32_at(args, 2)?,
                f32_at(args, 3)?,
            )
        };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "vertexAttrib4f", move |_h, _e, gl, args, _c| {
        unsafe {
            gl.vertex_attrib_4_f32(
                u32_at(args, 0)?,
                f32_at(args, 1)?,
                f32_at(args, 2)?,
                f32_at(args, 3)?,
                f32_at(args, 4)?,
            )
        };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "vertexAttrib1fv", move |_h, _e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if v.is_empty() {
            return Err(type_err("vertexAttrib1fv requires at least 1 value"));
        }
        unsafe { gl.vertex_attrib_1_f32_slice(u32_at(args, 0)?, &v) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "vertexAttrib2fv", move |_h, _e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if v.len() < 2 {
            return Err(type_err("vertexAttrib2fv requires at least 2 values"));
        }
        unsafe { gl.vertex_attrib_2_f32_slice(u32_at(args, 0)?, &v) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "vertexAttrib3fv", move |_h, _e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if v.len() < 3 {
            return Err(type_err("vertexAttrib3fv requires at least 3 values"));
        }
        unsafe { gl.vertex_attrib_3_f32_slice(u32_at(args, 0)?, &v) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "vertexAttrib4fv", move |_h, _e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if v.len() < 4 {
            return Err(type_err("vertexAttrib4fv requires at least 4 values"));
        }
        unsafe { gl.vertex_attrib_4_f32_slice(u32_at(args, 0)?, &v) };
        Ok(JsValue::undefined())
    });

    // ── Drawing ──
    gl_method!(cx, obj, "drawArrays", move |_h, _e, gl, args, _c| {
        let (mode, first, count) = (u32_at(args, 0)?, i32_at(args, 1)?, i32_at(args, 2)?);
        unsafe { gl.draw_arrays(mode, first, count) };
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "drawElements", move |_h, e, gl, args, _c| {
        let (mode, count, ty) = (u32_at(args, 0)?, i32_at(args, 1)?, u32_at(args, 2)?);
        let offset = i32_at(args, 3)?;
        // WebGL drawElements offsets are element counts; GL wants bytes.
        let Some(elt) = index_type_size(ty) else {
            e.ctx.push_error(types::INVALID_ENUM);
            return Ok(JsValue::undefined());
        };
        unsafe { gl.draw_elements(mode, count, ty, offset * elt) };
        Ok(JsValue::undefined())
    });

    // ── Uniforms ──
    gl_method!(cx, obj, "uniform1f", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_1_f32(Some(&loc), f32_at(args, 1)?) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform2f", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_2_f32(Some(&loc), f32_at(args, 1)?, f32_at(args, 2)?) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform3f", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe {
                gl.uniform_3_f32(
                    Some(&loc),
                    f32_at(args, 1)?,
                    f32_at(args, 2)?,
                    f32_at(args, 3)?,
                )
            };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform4f", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe {
                gl.uniform_4_f32(
                    Some(&loc),
                    f32_at(args, 1)?,
                    f32_at(args, 2)?,
                    f32_at(args, 3)?,
                    f32_at(args, 4)?,
                )
            };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform1i", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_1_i32(Some(&loc), i32_at(args, 1)?) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform2i", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_2_i32(Some(&loc), i32_at(args, 1)?, i32_at(args, 2)?) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform3i", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe {
                gl.uniform_3_i32(
                    Some(&loc),
                    i32_at(args, 1)?,
                    i32_at(args, 2)?,
                    i32_at(args, 3)?,
                )
            };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform4i", move |h, e, gl, args, c| {
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe {
                gl.uniform_4_i32(
                    Some(&loc),
                    i32_at(args, 1)?,
                    i32_at(args, 2)?,
                    i32_at(args, 3)?,
                    i32_at(args, 4)?,
                )
            };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform1fv", move |h, e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_1_f32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform2fv", move |h, e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_2_f32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform3fv", move |h, e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_3_f32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform4fv", move |h, e, gl, args, c| {
        let v = f32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_4_f32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform1iv", move |h, e, gl, args, c| {
        let v = i32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_1_i32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform2iv", move |h, e, gl, args, c| {
        let v = i32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_2_i32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform3iv", move |h, e, gl, args, c| {
        let v = i32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_3_i32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniform4iv", move |h, e, gl, args, c| {
        let v = i32_sequence(args.get_or_undefined(1), c)?;
        if let Some(loc) = uniform_location(e, h, args, c)? {
            unsafe { gl.uniform_4_i32_slice(Some(&loc), &v) };
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "uniformMatrix2fv", move |_h, e, gl, args, c| {
        matrix_uniform(e, args, c, |loc, t, v| unsafe {
            gl.uniform_matrix_2_f32_slice(loc, t, v)
        })
    });
    gl_method!(cx, obj, "uniformMatrix3fv", move |_h, e, gl, args, c| {
        matrix_uniform(e, args, c, |loc, t, v| unsafe {
            gl.uniform_matrix_3_f32_slice(loc, t, v)
        })
    });
    gl_method!(cx, obj, "uniformMatrix4fv", move |_h, e, gl, args, c| {
        matrix_uniform(e, args, c, |loc, t, v| unsafe {
            gl.uniform_matrix_4_f32_slice(loc, t, v)
        })
    });

    // ── Readback ──
    gl_method!(cx, obj, "readPixels", move |_h, e, gl, args, c| {
        let (x, y, w, h2) = (
            i32_at(args, 0)?,
            i32_at(args, 1)?,
            i32_at(args, 2)?,
            i32_at(args, 3)?,
        );
        let (format, ty) = (u32_at(args, 4)?, u32_at(args, 5)?);
        // WebGL1 default-framebuffer readback is RGBA/UNSIGNED_BYTE only.
        if format != glow::RGBA || ty != glow::UNSIGNED_BYTE {
            e.ctx.push_error(types::INVALID_ENUM);
            return Ok(JsValue::undefined());
        }
        if w < 0 || h2 < 0 {
            e.ctx.push_error(types::INVALID_VALUE);
            return Ok(JsValue::undefined());
        }
        let required = (w as usize) * (h2 as usize) * 4;
        // Size-check the destination BEFORE touching GL: an undersized
        // destination is INVALID_OPERATION and must not allocate.
        let Some(obj) = args.get_or_undefined(6).as_object() else {
            return Err(type_err(
                "readPixels requires a destination ArrayBufferView",
            ));
        };
        let ta = JsTypedArray::from_object(obj)?;
        let offset = ta.byte_offset(c)?;
        let len = ta.byte_length(c)?;
        if offset + required > len {
            e.ctx.push_error(types::INVALID_OPERATION);
            return Ok(JsValue::undefined());
        }
        let mut tmp = vec![0u8; required];
        unsafe {
            gl.read_pixels(
                x,
                y,
                w,
                h2,
                format,
                ty,
                glow::PixelPackData::Slice(Some(&mut tmp)),
            )
        };
        if !write_bytes_into_view(args.get_or_undefined(6), &tmp, c)? {
            e.ctx.push_error(types::INVALID_OPERATION);
        }
        Ok(JsValue::undefined())
    });

    // ── Queries & diagnostics ──
    gl_method!(cx, obj, "getError", move |_h, e, gl, _args, _c| {
        // Frontend queue first, then ANGLE's own GL error state so backend
        // validation errors stay visible (one error per call, per spec).
        if let Some(code) = e.ctx.errors.pop_front() {
            return Ok(JsValue::from(code as f64));
        }
        Ok(JsValue::from(unsafe { gl.get_error() } as f64))
    });
    gl_method!(cx, obj, "isContextLost", move |_h, _e, _gl, _args, _c| {
        // Contexts in the registry are alive by definition; lost is reported
        // through the error queue once make_current fails (then the entry is
        // treated as stale). ANGLE robust-init surfaces reset via EGL errors.
        Ok(JsValue::from(false))
    });
    gl_method!(
        cx,
        obj,
        "getContextAttributes",
        move |_h, e, _gl, _args, c| {
            let attrs = &e.ctx.attrs;
            let out = boa_engine::object::JsObject::with_object_proto(c.intrinsics());
            let pairs: [(&str, bool); 8] = [
                ("alpha", attrs.alpha),
                ("antialias", attrs.antialias),
                ("depth", attrs.depth),
                ("desynchronized", attrs.desynchronized),
                (
                    "failIfMajorPerformanceCaveat",
                    attrs.fail_if_major_performance_caveat,
                ),
                ("premultipliedAlpha", attrs.premultiplied_alpha),
                ("preserveDrawingBuffer", attrs.preserve_drawing_buffer),
                ("stencil", attrs.stencil),
            ];
            for (name, val) in pairs {
                out.set(js_string!(name), JsValue::from(val), false, c)?;
            }
            Ok(JsValue::from(out))
        }
    );
    gl_method!(cx, obj, "getParameter", move |h, e, gl, args, c| {
        let pname = u32_at(args, 0)?;
        Ok(get_parameter_value(h, e, gl, pname, c))
    });
    gl_method!(
        cx,
        obj,
        "getSupportedExtensions",
        move |_h, _e, gl, _args, c| {
            let raw = unsafe { gl.get_parameter_string(glow::EXTENSIONS) };
            let names: Vec<JsValue> = raw
                .split_whitespace()
                .filter_map(|token| token.strip_prefix("GL_"))
                .filter(|name| {
                    name.starts_with("OES_")
                        || name.starts_with("EXT_")
                        || name.starts_with("ANGLE_")
                        || name.starts_with("WEBGL_")
                })
                .map(JsString::from)
                .map(JsValue::from)
                .collect();
            Ok(JsValue::from(JsArray::from_iter(names, c)))
        }
    );
    gl_method!(cx, obj, "getExtension", move |_h, _e, gl, args, c| {
        let name = str_at(args, 0)?;
        // WEBGL_debug_renderer_info always exists (Chrome always exposes it);
        // it carries the UNMASKED_* constants.
        if name == "WEBGL_debug_renderer_info" {
            let o = JsObject::with_object_proto(c.intrinsics());
            let _ = o.set(
                js_string!("UNMASKED_VENDOR_WEBGL"),
                JsValue::from(37445.0),
                false,
                c,
            );
            let _ = o.set(
                js_string!("UNMASKED_RENDERER_WEBGL"),
                JsValue::from(37446.0),
                false,
                c,
            );
            return Ok(JsValue::from(o));
        }
        // In WebGL2 the WebGL1 extension set below is core; report null.
        if kind == CtxKind::WebGl2 {
            return Ok(JsValue::null());
        }
        if !EXTENSION_ALLOWLIST.contains(&name.as_str()) {
            return Ok(JsValue::null());
        }
        // Honest exposure: only when ANGLE advertises the backing GL extension.
        let raw = unsafe { gl.get_parameter_string(glow::EXTENSIONS) };
        let gl_name = format!("GL_{name}");
        if raw.split_whitespace().any(|t| t == gl_name) {
            Ok(JsValue::from(JsObject::with_object_proto(c.intrinsics())))
        } else {
            Ok(JsValue::null())
        }
    });

    // ── Deletions & predicates ──
    gl_method!(cx, obj, "deleteShader", move |_h, e, gl, args, c| {
        delete_resource(e, gl, args, c, ResourceKind::Shader, |gl, s| unsafe {
            gl.delete_shader(glow::NativeShader(nz(s)))
        });
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "deleteProgram", move |_h, e, gl, args, c| {
        delete_resource(e, gl, args, c, ResourceKind::Program, |gl, p| unsafe {
            gl.delete_program(glow::NativeProgram(nz(p)))
        });
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "deleteBuffer", move |_h, e, gl, args, c| {
        let id = delete_resource(e, gl, args, c, ResourceKind::Buffer, |gl, b| unsafe {
            gl.delete_buffer(glow::NativeBuffer(nz(b)))
        });
        // Spec: deleting a buffer unbinds it from binding points.
        if e.ctx.bound_array_buffer == id {
            e.ctx.bound_array_buffer = None;
        }
        if e.ctx.bound_element_array_buffer == id {
            e.ctx.bound_element_array_buffer = None;
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "deleteTexture", move |_h, e, gl, args, c| {
        let id = delete_resource(e, gl, args, c, ResourceKind::Texture, |gl, t| unsafe {
            gl.delete_texture(glow::NativeTexture(nz(t)))
        });
        if e.ctx.bound_texture_2d == id {
            e.ctx.bound_texture_2d = None;
        }
        if e.ctx.bound_texture_cube_map == id {
            e.ctx.bound_texture_cube_map = None;
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "deleteFramebuffer", move |_h, e, gl, args, c| {
        delete_resource(e, gl, args, c, ResourceKind::Framebuffer, |gl, f| unsafe {
            gl.delete_framebuffer(glow::NativeFramebuffer(nz(f)))
        });
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "deleteRenderbuffer", move |_h, e, gl, args, c| {
        delete_resource(e, gl, args, c, ResourceKind::Renderbuffer, |gl, r| unsafe {
            gl.delete_renderbuffer(glow::NativeRenderbuffer(nz(r)))
        });
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "isShader", move |h, e, _gl, args, c| {
        Ok(JsValue::from(is_live_resource(
            e,
            h,
            args.get_or_undefined(0),
            c,
            ResourceKind::Shader,
        )))
    });
    gl_method!(cx, obj, "isProgram", move |h, e, _gl, args, c| {
        Ok(JsValue::from(is_live_resource(
            e,
            h,
            args.get_or_undefined(0),
            c,
            ResourceKind::Program,
        )))
    });
    gl_method!(cx, obj, "isBuffer", move |h, e, _gl, args, c| {
        Ok(JsValue::from(is_live_resource(
            e,
            h,
            args.get_or_undefined(0),
            c,
            ResourceKind::Buffer,
        )))
    });
    gl_method!(cx, obj, "isTexture", move |h, e, _gl, args, c| {
        Ok(JsValue::from(is_live_resource(
            e,
            h,
            args.get_or_undefined(0),
            c,
            ResourceKind::Texture,
        )))
    });
    gl_method!(cx, obj, "isFramebuffer", move |h, e, _gl, args, c| {
        Ok(JsValue::from(is_live_resource(
            e,
            h,
            args.get_or_undefined(0),
            c,
            ResourceKind::Framebuffer,
        )))
    });
    gl_method!(cx, obj, "isRenderbuffer", move |h, e, _gl, args, c| {
        Ok(JsValue::from(is_live_resource(
            e,
            h,
            args.get_or_undefined(0),
            c,
            ResourceKind::Renderbuffer,
        )))
    });

    // ── WebGL2-only surface (absent on WebGL1 contexts, per spec) ──
    if kind == CtxKind::WebGl2 {
        gl_method!(cx, obj, "createVertexArray", move |h, e, gl, _args, c| {
            match unsafe { gl.create_vertex_array() } {
                Ok(vao) => Ok(new_resource(
                    c,
                    e,
                    h,
                    ResourceKind::VertexArray,
                    vao.0.get(),
                )),
                Err(_) => {
                    e.ctx.push_error(types::OUT_OF_MEMORY);
                    Ok(JsValue::undefined())
                }
            }
        });
        gl_method!(cx, obj, "bindVertexArray", move |h, e, gl, args, c| {
            let v = args.get_or_undefined(0);
            let native = if v.is_null_or_undefined() {
                None
            } else {
                resolve_resource(e, h, v, c, ResourceKind::VertexArray)?
                    .map(|vao| glow::NativeVertexArray(nz(vao)))
            };
            unsafe { gl.bind_vertex_array(native) };
            Ok(JsValue::undefined())
        });
        gl_method!(cx, obj, "deleteVertexArray", move |_h, e, gl, args, c| {
            delete_resource(
                e,
                gl,
                args,
                c,
                ResourceKind::VertexArray,
                |gl, vao| unsafe { gl.delete_vertex_array(glow::NativeVertexArray(nz(vao))) },
            );
            Ok(JsValue::undefined())
        });
        gl_method!(cx, obj, "isVertexArray", move |h, e, _gl, args, c| {
            Ok(JsValue::from(is_live_resource(
                e,
                h,
                args.get_or_undefined(0),
                c,
                ResourceKind::VertexArray,
            )))
        });
        gl_method!(
            cx,
            obj,
            "drawArraysInstanced",
            move |_h, _e, gl, args, _c| {
                let (mode, first, count, instance_count) = (
                    u32_at(args, 0)?,
                    i32_at(args, 1)?,
                    i32_at(args, 2)?,
                    i32_at(args, 3)?,
                );
                unsafe { gl.draw_arrays_instanced(mode, first, count, instance_count) };
                Ok(JsValue::undefined())
            }
        );
        gl_method!(
            cx,
            obj,
            "drawElementsInstanced",
            move |_h, e, gl, args, _c| {
                let (mode, count, ty, offset, instance_count) = (
                    u32_at(args, 0)?,
                    i32_at(args, 1)?,
                    u32_at(args, 2)?,
                    i32_at(args, 3)?,
                    i32_at(args, 4)?,
                );
                let Some(elt) = index_type_size(ty) else {
                    e.ctx.push_error(types::INVALID_ENUM);
                    return Ok(JsValue::undefined());
                };
                unsafe {
                    gl.draw_elements_instanced(mode, count, ty, offset * elt, instance_count)
                };
                Ok(JsValue::undefined())
            }
        );
        gl_method!(
            cx,
            obj,
            "vertexAttribDivisor",
            move |_h, _e, gl, args, _c| {
                let (index, divisor) = (u32_at(args, 0)?, u32_at(args, 1)?);
                unsafe { gl.vertex_attrib_divisor(index, divisor) };
                Ok(JsValue::undefined())
            }
        );
    }
}

// ─── Shared method internals ────────────────────────────────────────────────

/// Resolves the `UniformLocation` wrapper in `args[0]` for a uniform setter.
/// `null`/`undefined` location is silently ignored per spec (`Ok(None)`);
/// a non-wrapper type is a `TypeError`; wrong context/kind/deleted enqueues
/// `INVALID_OPERATION` and resolves to `Ok(None)`.
fn uniform_location(
    entry: &mut RegistryEntry,
    this_handle: u64,
    args: &[JsValue],
    cx: &mut Context,
) -> Result<Option<glow::NativeUniformLocation>, JsError> {
    let v = args.get_or_undefined(0);
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    match resolve_resource(entry, this_handle, v, cx, ResourceKind::UniformLocation)? {
        Some(handle) => Ok(Some(glow::NativeUniformLocation(handle))),
        None => Ok(None),
    }
}

/// Shared body of `uniformMatrix{2,3,4}fv`. WebGL1 forbids `transpose: true`
/// (`INVALID_VALUE`); WebGL2 forwards it.
fn matrix_uniform(
    entry: &mut RegistryEntry,
    args: &[JsValue],
    cx: &mut Context,
    upload: impl FnOnce(Option<&glow::NativeUniformLocation>, bool, &[f32]),
) -> JsResult<JsValue> {
    let transpose = args.get_or_undefined(1).to_boolean();
    if transpose && entry.kind != CtxKind::WebGl2 {
        entry.ctx.push_error(types::INVALID_VALUE);
        return Ok(JsValue::undefined());
    }
    let v = f32_sequence(args.get_or_undefined(2), cx)?;
    // Null location still accepts the data (silently unused, per GL spec).
    let loc = uniform_location(entry, entry.handle, args, cx)?;
    upload(loc.as_ref(), transpose, &v);
    Ok(JsValue::undefined())
}

/// Shared `delete*` body. Returns the frontend resource id that was deleted
/// (for mirror unbinding), or `None` (null arg → no-op; error already queued).
fn delete_resource(
    entry: &mut RegistryEntry,
    gl: &glow::Context,
    args: &[JsValue],
    cx: &mut Context,
    expected: ResourceKind,
    delete: impl FnOnce(&glow::Context, u32),
) -> Option<u32> {
    let v = args.get_or_undefined(0);
    if v.is_null_or_undefined() {
        return None;
    }
    let Some((owner, id, kind)) = unwrap_wrapper(v, cx) else {
        entry.ctx.push_error(types::INVALID_OPERATION);
        return None;
    };
    if owner != entry.handle {
        entry.ctx.push_error(types::INVALID_OPERATION);
        return None;
    }
    if kind != expected {
        entry.ctx.push_error(types::INVALID_OPERATION);
        return None;
    }
    let Some(res) = entry.ctx.resource(id) else {
        entry.ctx.push_error(types::INVALID_VALUE);
        return None;
    };
    if res.deleted {
        // Deleting twice is a no-op (spec: already-deleted objects are ignored).
        return None;
    }
    delete(gl, res.handle);
    entry.ctx.mark_deleted(id);
    Some(id)
}

/// `isX()` semantics: true only when the wrapper belongs to this context, has
/// the right kind, is known to the frontend, and is not deleted. Foreign junk
/// returns false (not an error), matching browser behavior for `is*` checks.
fn is_live_resource(
    entry: &mut RegistryEntry,
    this_handle: u64,
    v: &JsValue,
    cx: &mut Context,
    expected: ResourceKind,
) -> bool {
    let Some((owner, id, kind)) = unwrap_wrapper(v, cx) else {
        return false;
    };
    if owner != this_handle || kind != expected {
        return false;
    }
    matches!(entry.ctx.resource(id), Some(res) if !res.deleted)
}

/// `getParameter` — mirrors first (binding state, WebGL-only knobs, identity),
/// then a typed table forwarding to glow, else `INVALID_ENUM`.
fn get_parameter_value(
    handle: u64,
    entry: &mut RegistryEntry,
    gl: &glow::Context,
    pname: u32,
    cx: &mut Context,
) -> JsValue {
    let fctx = &mut entry.ctx;
    match pname {
        // Binding mirrors (frontend resource ids → wrappers).
        0x8894 => binding_wrapper(cx, handle, fctx.bound_array_buffer, ResourceKind::Buffer),
        0x8895 => binding_wrapper(
            cx,
            handle,
            fctx.bound_element_array_buffer,
            ResourceKind::Buffer,
        ),
        0x8CA6 => binding_wrapper(
            cx,
            handle,
            fctx.bound_framebuffer,
            ResourceKind::Framebuffer,
        ),
        0x8CA7 => binding_wrapper(
            cx,
            handle,
            fctx.bound_renderbuffer,
            ResourceKind::Renderbuffer,
        ),
        0x8069 => binding_wrapper(cx, handle, fctx.bound_texture_2d, ResourceKind::Texture),
        0x8514 => binding_wrapper(
            cx,
            handle,
            fctx.bound_texture_cube_map,
            ResourceKind::Texture,
        ),
        0x8B8D => binding_wrapper(cx, handle, fctx.used_program, ResourceKind::Program),
        // ACTIVE_TEXTURE
        0x84E0 => JsValue::from((glow::TEXTURE0 + fctx.active_texture_unit) as f64),
        // WebGL-only pixel-store mirrors.
        0x9240 => JsValue::from(fctx.unpack_flip_y),
        0x9241 => JsValue::from(fctx.unpack_premultiply),
        // Colorspace conversion is not applied on the CPU path; report the
        // spec default (BROWSER_DEFAULT_WEBGL).
        0x9243 => JsValue::from(0x9244 as f64),
        // Spoofed identity (kept coherent with the stealth profile).
        37445 => JsValue::from(JsString::from(fctx.identity.0.as_str())),
        37446 => JsValue::from(JsString::from(fctx.identity.1.as_str())),
        // Strings
        0x1F00 | 0x1F01 | 0x1F02 | 0x8B8C => {
            JsValue::from(JsString::from(unsafe { gl.get_parameter_string(pname) }))
        }
        // Boolean caps
        0x0BE2 | 0x0B44 | 0x0B71 | 0x0B72 | 0x0BD0 | 0x8037 | 0x809E | 0x80A0 | 0x80AB | 0x0C11
        | 0x0B90 => JsValue::from(unsafe { gl.get_parameter_bool(pname) }),
        // Single floats
        0x0B73 | 0x0B21 | 0x8038 | 0x2A00 | 0x80AA => {
            JsValue::from(unsafe { gl.get_parameter_f32(pname) } as f64)
        }
        // Float ranges → Float32Array
        0x846D | 0x846E | 0x0C22 | 0x0B70 => {
            let mut v = [0.0f32; 4];
            unsafe { gl.get_parameter_f32_slice(pname, &mut v) };
            match JsFloat32Array::from_iter(v.to_vec(), cx) {
                Ok(a) => JsValue::from(a),
                Err(_) => JsValue::undefined(),
            }
        }
        // Int vectors → array of numbers
        0x0BA2 | 0x0C10 | 0x0D3A => {
            let mut v = [0i32; 4];
            unsafe { gl.get_parameter_i32_slice(pname, &mut v) };
            JsValue::from(JsArray::from_iter(
                v.iter().map(|n| JsValue::from(*n as f64)),
                cx,
            ))
        }
        // COMPRESSED_TEXTURE_FORMATS (variable length; trailing zeros trimmed).
        0x86A3 => {
            let mut v = [0i32; 128];
            unsafe { gl.get_parameter_i32_slice(pname, &mut v) };
            JsValue::from(JsArray::from_iter(
                v.iter()
                    .take_while(|n| **n != 0)
                    .map(|n| JsValue::from(*n as f64)),
                cx,
            ))
        }
        // 4-element boolean vector → array
        0x0C23 => {
            let v = unsafe { gl.get_parameter_bool_array::<4>(pname) };
            JsValue::from(JsArray::from_iter(v.iter().map(|b| JsValue::from(*b)), cx))
        }
        // Integer parameters (the GL spec table for WebGL1 plus the common
        // WebGL2 subset; unknown values fall through to INVALID_ENUM).
        0x0D33 | 0x851C | 0x84E8 | 0x8869 | 0x8DFB | 0x8DFC | 0x8DFD | 0x8B4C | 0x8B4D | 0x8872
        | 0x0D50 | 0x0D52 | 0x0D53 | 0x0D54 | 0x0D55 | 0x0D56 | 0x0D57 | 0x80A9 | 0x80A8
        | 0x0B97 | 0x8CA3 | 0x0B9A | 0x8CA4 | 0x0B98 | 0x8CA5 | 0x0CF5 | 0x0D05 | 0x8B3A
        | 0x8B3B | 0x8192 | 0x8009 | 0x883D | 0x80C9 | 0x80C8 | 0x80CB | 0x80CA | 0x0B74
        | 0x0B45 | 0x0B46 | 0x0B92 | 0x0B94 | 0x0B95 | 0x0B96 | 0x8800 | 0x8801 | 0x8802
        | 0x8803 | 0x80E9 | 0x80E8 | 0x88FF | 0x8CDF | 0x8824 | 0x8D57 | 0x8073 => {
            JsValue::from(unsafe { gl.get_parameter_i32(pname) } as f64)
        }
        _ => {
            fctx.push_error(types::INVALID_ENUM);
            JsValue::undefined()
        }
    }
}

// ─── Context object construction ────────────────────────────────────────────

/// Builds the object returned by `canvas.getContext(...)`: canvas back-ref,
/// live drawing-buffer accessors, every method as an own property, and the
/// full WebGL constant table (plus the WebGL2 set for WebGL2 contexts).
///
/// `webgl_identity` is accepted for signature symmetry with `get_context`;
/// the live spoofed identity is read from `FrontContext.identity` (the same
/// tuple) inside `getParameter`, so a stale copy is never consulted.
pub fn build_context_object(
    cx: &mut Context,
    handle: u64,
    kind: CtxKind,
    canvas: JsValue,
    _webgl_identity: (String, String),
) -> JsObject {
    let obj = JsObject::with_object_proto(cx.intrinsics());
    let _ = obj.set(js_string!("canvas"), canvas, false, cx);
    // Hidden dispatch key (read by every method closure).
    let _ = obj.set(
        JsString::from(CTX_HANDLE_PROP),
        JsValue::from(handle as f64),
        false,
        cx,
    );

    // Live drawing-buffer dimensions; 0 when the context is lost (spec).
    for (name, index) in [
        ("drawingBufferWidth", 0usize),
        ("drawingBufferHeight", 1usize),
    ] {
        let getter = FunctionObjectBuilder::new(cx.realm(), unsafe {
            NativeFunction::from_closure(move |_this: &JsValue, _args: &[JsValue], _cx| {
                let dims = super::find_by_handle(handle).and_then(|key| {
                    super::with_entry(key, |e| (e.ctx.backend.width, e.ctx.backend.height))
                });
                let v = match dims {
                    Some((w, h)) => {
                        if index == 0 {
                            w
                        } else {
                            h
                        }
                    }
                    None => 0,
                };
                Ok(JsValue::from(v as f64))
            })
        })
        .name(JsString::from(format!("get {name}")))
        .build();
        let desc = PropertyDescriptor::builder()
            .get(getter)
            .enumerable(true)
            .configurable(true);
        let _ = obj.define_property_or_throw(JsString::from(name), desc, cx);
    }

    register_methods(cx, &obj, kind);

    for (name, value) in CONSTANTS {
        let _ = obj.set(JsString::from(*name), JsValue::from(*value), false, cx);
    }
    // TEXTURE0..TEXTURE31.
    for unit in 0..32u32 {
        let _ = obj.set(
            JsString::from(format!("TEXTURE{unit}")),
            JsValue::from((0x84C0 + unit) as f64),
            false,
            cx,
        );
    }
    if kind == CtxKind::WebGl2 {
        for (name, value) in CONSTANTS_WEBGL2 {
            let _ = obj.set(JsString::from(*name), JsValue::from(*value), false, cx);
        }
    }
    obj
}
