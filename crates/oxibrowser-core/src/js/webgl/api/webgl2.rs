//! WebGL2 uniform buffers and core queries. No timer-query extension is exposed.
//!
//! Query result types follow Khronos WebGL2 GET_QUERY_PARAMETER: the result is
//! GLuint (including occlusion 0/1), and availability is GLboolean.

use super::*;

const OBJECTS: &str = "__webgl2Objects";

/// WebIDL GLuint / GLenum conversion wraps modulo 2^32; Rust's float cast
/// saturates instead and would turn a negative binding index into valid zero.
fn unsigned_at(args: &[JsValue], index: usize, cx: &mut Context) -> JsResult<u32> {
    args.get_or_undefined(index).to_u32(cx)
}

/// Keep wrapper identity in a traced JS object, never an unsafe Rust closure.
fn remember(entry: &RegistryEntry, value: &JsValue, cx: &mut Context) -> JsResult<()> {
    let Some((_, id, _)) = unwrap_wrapper(value, cx) else {
        return Ok(());
    };
    let cache = match entry.js.get(JsString::from(OBJECTS), cx)?.as_object() {
        Some(cache) => cache,
        None => {
            let cache = JsObject::with_null_proto();
            entry
                .js
                .create_data_property_or_throw(JsString::from(OBJECTS), cache.clone(), cx)?;
            cache
        }
    };
    cache.create_data_property_or_throw(id, value.clone(), cx)?;
    Ok(())
}

pub(super) fn forget(entry: &RegistryEntry, id: Option<u32>, cx: &mut Context) -> JsResult<()> {
    if let Some(id) = id
        && let Some(cache) = entry.js.get(JsString::from(OBJECTS), cx)?.as_object()
    {
        cache.delete_property_or_throw(id, cx)?;
    }
    Ok(())
}

fn wrapped_native(
    entry: &RegistryEntry,
    kind: ResourceKind,
    native: u32,
    cx: &mut Context,
) -> JsResult<JsValue> {
    if native == 0 {
        return Ok(JsValue::null());
    }
    let Some(id) = entry.ctx.resources.iter().find_map(|(id, resource)| {
        (resource.kind == kind && resource.handle == native && !resource.deleted).then_some(*id)
    }) else {
        return Ok(JsValue::null());
    };
    if let Some(cache) = entry.js.get(JsString::from(OBJECTS), cx)?.as_object() {
        let value = cache.get(id, cx)?;
        if !value.is_undefined() {
            return Ok(value);
        }
    }
    Ok(binding_wrapper(cx, entry.handle, Some(id), kind))
}

fn query_target(target: u32) -> bool {
    matches!(
        target,
        glow::ANY_SAMPLES_PASSED
            | glow::ANY_SAMPLES_PASSED_CONSERVATIVE
            | glow::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN
    )
}

fn current_query(target: u32) -> Option<u32> {
    // GLES3 glGetQueryiv is absent from glow 0.16's public trait. Resolve the
    // real entry point from the same pinned ANGLE that owns these contexts.
    let pointer =
        unsafe { oxibrowser_render::gl::egl::eglGetProcAddress(c"glGetQueryiv".as_ptr()) };
    if pointer.is_null() {
        return None;
    }
    let get: unsafe extern "C" fn(u32, u32, *mut i32) = unsafe { std::mem::transmute(pointer) };
    let mut name = 0;
    unsafe {
        get(target, glow::CURRENT_QUERY, &mut name);
    }
    Some(name as u32)
}

fn native_is_query(name: u32) -> bool {
    let pointer = unsafe { oxibrowser_render::gl::egl::eglGetProcAddress(c"glIsQuery".as_ptr()) };
    if pointer.is_null() {
        return false;
    }
    let is: unsafe extern "C" fn(u32) -> u8 = unsafe { std::mem::transmute(pointer) };
    unsafe { is(name) != 0 }
}

fn query_value(name: u32, pname: u32) -> Option<u32> {
    // glow 0.16 tests the unrelated desktop GetQueryBufferObjectiv entry point
    // to choose between core and EXT query retrieval. GLES3 must use core:
    // the EXT symbol can exist while its extension is disabled in WebGL mode.
    let pointer =
        unsafe { oxibrowser_render::gl::egl::eglGetProcAddress(c"glGetQueryObjectuiv".as_ptr()) };
    if pointer.is_null() {
        return None;
    }
    let get: unsafe extern "C" fn(u32, u32, *mut u32) = unsafe { std::mem::transmute(pointer) };
    let mut value = 0;
    unsafe { get(name, pname, &mut value) };
    Some(value)
}

pub(super) fn register(cx: &mut Context, obj: &JsObject) {
    gl_method!(cx, obj, "getUniformBlockIndex", move |h, e, gl, args, c| {
        let Some(program) =
            resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        else {
            return Ok(JsValue::from(u32::MAX));
        };
        let name = str_at(args, 1)?;
        let index = unsafe { gl.get_uniform_block_index(glow::NativeProgram(nz(program)), &name) };
        Ok(JsValue::from(index.unwrap_or(u32::MAX)))
    });
    gl_method!(cx, obj, "uniformBlockBinding", unsigned [1, 2], move |h, e, gl, args, c| {
        let Some(program) =
            resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Program)?
        else {
            return Ok(JsValue::undefined());
        };
        let index = unsigned_at(args, 1, c)?;
        let binding = unsigned_at(args, 2, c)?;
        unsafe {
            gl.uniform_block_binding(glow::NativeProgram(nz(program)), index, binding);
        }
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "bindBufferBase", unsigned [0, 1], move |h, e, gl, args, c| {
        let target = unsigned_at(args, 0, c)?;
        if target != glow::UNIFORM_BUFFER {
            e.ctx.push_error(types::INVALID_ENUM);
            return Ok(JsValue::undefined());
        }
        let index = unsigned_at(args, 1, c)?;
        let limit = unsafe { gl.get_parameter_i32(glow::MAX_UNIFORM_BUFFER_BINDINGS) } as u32;
        if index >= limit {
            e.ctx.push_error(types::INVALID_VALUE);
            return Ok(JsValue::undefined());
        }
        let value = args.get_or_undefined(2);
        let (native, id) = if value.is_null_or_undefined() {
            (None, None)
        } else {
            let Some(name) = resolve_resource(e, h, value, c, ResourceKind::Buffer)? else {
                return Ok(JsValue::undefined());
            };
            remember(e, value, c)?;
            (
                Some(glow::NativeBuffer(nz(name))),
                unwrap_wrapper(value, c).map(|(_, id, _)| id),
            )
        };
        unsafe {
            gl.bind_buffer_base(target, index, native);
        }
        e.ctx.bound_uniform_buffer = id;
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "getIndexedParameter", unsigned [0, 1], move |_h, e, gl, args, c| {
        let target = unsigned_at(args, 0, c)?;
        let index = unsigned_at(args, 1, c)?;
        if !matches!(
            target,
            glow::UNIFORM_BUFFER_BINDING | glow::UNIFORM_BUFFER_START | glow::UNIFORM_BUFFER_SIZE
        ) {
            e.ctx.push_error(types::INVALID_ENUM);
            return Ok(JsValue::null());
        }
        let limit = unsafe { gl.get_parameter_i32(glow::MAX_UNIFORM_BUFFER_BINDINGS) } as u32;
        if index >= limit {
            e.ctx.push_error(types::INVALID_VALUE);
            return Ok(JsValue::null());
        }
        if target == glow::UNIFORM_BUFFER_BINDING {
            let name = unsafe { gl.get_parameter_indexed_i32(target, index) } as u32;
            return wrapped_native(e, ResourceKind::Buffer, name, c);
        }
        let mut value = unsafe { gl.get_parameter_indexed_i64(target, index) };
        if target == glow::UNIFORM_BUFFER_SIZE && value == 0 {
            let indexed =
                unsafe { gl.get_parameter_indexed_i32(glow::UNIFORM_BUFFER_BINDING, index) } as u32;
            if indexed != 0 {
                // ANGLE represents bindBufferBase's whole-buffer range with a
                // zero-size sentinel. Reflect its actual current storage size,
                // restoring the generic binding without changing its mirror.
                let previous = unsafe { gl.get_parameter_i32(glow::UNIFORM_BUFFER_BINDING) } as u32;
                unsafe {
                    gl.bind_buffer(glow::UNIFORM_BUFFER, Some(glow::NativeBuffer(nz(indexed))));
                    value =
                        gl.get_buffer_parameter_i32(glow::UNIFORM_BUFFER, glow::BUFFER_SIZE) as i64;
                    gl.bind_buffer(
                        glow::UNIFORM_BUFFER,
                        NonZeroU32::new(previous).map(glow::NativeBuffer),
                    );
                }
            }
        }
        Ok(JsValue::from(value as f64))
    });
    gl_method!(cx, obj, "createQuery", move |h, e, gl, _args, c| {
        match unsafe { gl.create_query() } {
            Ok(query) => {
                let value = new_resource(c, e, h, ResourceKind::Query, query.0.get());
                remember(e, &value, c)?;
                Ok(value)
            }
            Err(_) => {
                e.ctx.push_error(types::OUT_OF_MEMORY);
                Ok(JsValue::null())
            }
        }
    });
    gl_method!(cx, obj, "deleteQuery", move |_h, e, gl, args, c| {
        let id = delete_resource(e, gl, args, c, ResourceKind::Query, |gl, name| unsafe {
            gl.delete_query(glow::NativeQuery(nz(name)))
        });
        forget(e, id, c)?;
        Ok(JsValue::undefined())
    });
    gl_method!(cx, obj, "isQuery", move |h, e, _gl, args, c| {
        let value = args.get_or_undefined(0);
        if !is_live_resource(e, h, value, c, ResourceKind::Query) {
            return Ok(JsValue::from(false));
        }
        let name = unwrap_wrapper(value, c)
            .and_then(|(_, id, _)| e.ctx.resource(id))
            .map(|resource| resource.handle)
            .unwrap();
        Ok(JsValue::from(native_is_query(name)))
    });
    gl_method!(
        cx,
        obj,
        "beginQuery",
        unsigned[0],
        move |h, e, gl, args, c| {
            let target = unsigned_at(args, 0, c)?;
            if !query_target(target) {
                e.ctx.push_error(types::INVALID_ENUM);
                return Ok(JsValue::undefined());
            }
            let Some(name) =
                resolve_resource(e, h, args.get_or_undefined(1), c, ResourceKind::Query)?
            else {
                return Ok(JsValue::undefined());
            };
            unsafe {
                gl.begin_query(target, glow::NativeQuery(nz(name)));
            }
            Ok(JsValue::undefined())
        }
    );
    gl_method!(
        cx,
        obj,
        "endQuery",
        unsigned[0],
        move |_h, e, gl, args, c| {
            let target = unsigned_at(args, 0, c)?;
            if !query_target(target) {
                e.ctx.push_error(types::INVALID_ENUM);
                return Ok(JsValue::undefined());
            }
            unsafe {
                gl.end_query(target);
            }
            Ok(JsValue::undefined())
        }
    );
    gl_method!(cx, obj, "getQuery", unsigned [0, 1], move |_h, e, _gl, args, c| {
        let target = unsigned_at(args, 0, c)?;
        let pname = unsigned_at(args, 1, c)?;
        if !query_target(target) || pname != glow::CURRENT_QUERY {
            e.ctx.push_error(types::INVALID_ENUM);
            return Ok(JsValue::null());
        }
        match current_query(target) {
            Some(name) => wrapped_native(e, ResourceKind::Query, name, c),
            None => {
                e.ctx.push_error(types::INVALID_OPERATION);
                Ok(JsValue::null())
            }
        }
    });
    gl_method!(
        cx,
        obj,
        "getQueryParameter",
        unsigned[1],
        move |h, e, _gl, args, c| {
            let Some(name) =
                resolve_resource(e, h, args.get_or_undefined(0), c, ResourceKind::Query)?
            else {
                return Ok(JsValue::null());
            };
            let pname = unsigned_at(args, 1, c)?;
            if !matches!(pname, glow::QUERY_RESULT | glow::QUERY_RESULT_AVAILABLE) {
                e.ctx.push_error(types::INVALID_ENUM);
                return Ok(JsValue::null());
            }
            if !native_is_query(name)
                || [
                    glow::ANY_SAMPLES_PASSED,
                    glow::ANY_SAMPLES_PASSED_CONSERVATIVE,
                    glow::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN,
                ]
                .iter()
                .any(|target| current_query(*target) == Some(name))
            {
                e.ctx.push_error(types::INVALID_OPERATION);
                return Ok(JsValue::null());
            }
            let Some(value) = query_value(name, pname) else {
                e.ctx.push_error(types::INVALID_OPERATION);
                return Ok(JsValue::null());
            };
            Ok(if pname == glow::QUERY_RESULT_AVAILABLE {
                JsValue::from(value != 0)
            } else {
                JsValue::from(value)
            })
        }
    );
}

/// Parent getParameter dispatch uses this for UBO-specific native reflection.
pub(super) fn parameter(
    entry: &mut RegistryEntry,
    gl: &glow::Context,
    pname: u32,
    cx: &mut Context,
) -> Option<JsValue> {
    match pname {
        glow::UNIFORM_BUFFER_BINDING => {
            let name = unsafe { gl.get_parameter_i32(pname) } as u32;
            Some(wrapped_native(entry, ResourceKind::Buffer, name, cx).unwrap_or(JsValue::null()))
        }
        glow::MAX_VERTEX_UNIFORM_BLOCKS
        | glow::MAX_FRAGMENT_UNIFORM_BLOCKS
        | glow::MAX_COMBINED_UNIFORM_BLOCKS
        | glow::MAX_UNIFORM_BUFFER_BINDINGS
        | glow::UNIFORM_BUFFER_OFFSET_ALIGNMENT => {
            Some(JsValue::from(unsafe { gl.get_parameter_i32(pname) }))
        }
        glow::MAX_UNIFORM_BLOCK_SIZE => {
            Some(JsValue::from(unsafe { gl.get_parameter_i64(pname) } as f64))
        }
        _ => None,
    }
}

/// Call after a successful generic UNIFORM_BUFFER bind in the parent surface.
pub(super) fn remember_buffer(
    entry: &RegistryEntry,
    value: &JsValue,
    cx: &mut Context,
) -> JsResult<()> {
    remember(entry, value, cx)
}

pub(super) const CONSTANTS: &[(&str, u32)] = &[
    ("ANY_SAMPLES_PASSED", glow::ANY_SAMPLES_PASSED),
    (
        "ANY_SAMPLES_PASSED_CONSERVATIVE",
        glow::ANY_SAMPLES_PASSED_CONSERVATIVE,
    ),
    (
        "TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN",
        glow::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN,
    ),
    ("CURRENT_QUERY", glow::CURRENT_QUERY),
    ("QUERY_RESULT", glow::QUERY_RESULT),
    ("QUERY_RESULT_AVAILABLE", glow::QUERY_RESULT_AVAILABLE),
    ("MAX_VERTEX_UNIFORM_BLOCKS", glow::MAX_VERTEX_UNIFORM_BLOCKS),
    (
        "MAX_FRAGMENT_UNIFORM_BLOCKS",
        glow::MAX_FRAGMENT_UNIFORM_BLOCKS,
    ),
    (
        "MAX_COMBINED_UNIFORM_BLOCKS",
        glow::MAX_COMBINED_UNIFORM_BLOCKS,
    ),
    (
        "MAX_UNIFORM_BUFFER_BINDINGS",
        glow::MAX_UNIFORM_BUFFER_BINDINGS,
    ),
    ("MAX_UNIFORM_BLOCK_SIZE", glow::MAX_UNIFORM_BLOCK_SIZE),
    (
        "UNIFORM_BUFFER_OFFSET_ALIGNMENT",
        glow::UNIFORM_BUFFER_OFFSET_ALIGNMENT,
    ),
    ("UNIFORM_BUFFER_START", glow::UNIFORM_BUFFER_START),
    ("UNIFORM_BUFFER_SIZE", glow::UNIFORM_BUFFER_SIZE),
    ("INVALID_INDEX", u32::MAX),
    ("TEXTURE_COMPARE_FUNC", glow::TEXTURE_COMPARE_FUNC),
    ("TEXTURE_COMPARE_MODE", glow::TEXTURE_COMPARE_MODE),
    ("COMPARE_REF_TO_TEXTURE", glow::COMPARE_REF_TO_TEXTURE),
    ("TEXTURE_BASE_LEVEL", glow::TEXTURE_BASE_LEVEL),
    ("TEXTURE_MAX_LEVEL", glow::TEXTURE_MAX_LEVEL),
];
