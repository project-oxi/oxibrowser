//! Canvas-element JS bindings for WebGL: `getContext`, `toDataURL`, and the
//! width/height attribute reflection that drives drawing-buffer resize.
//!
//! Attached from `create_render_element_object` for `canvas` elements so
//! parsed and created canvases behave identically. The legacy JS bootstrap
//! shim skips elements marked `__oxiWebglNative`.

use std::cell::RefCell;
use std::rc::Rc;

use base64::Engine;
use boa_engine::native_function::NativeFunction;
use boa_engine::object::JsObject;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsArgs, JsError, JsNativeError, JsResult, JsValue, js_string};
use oxibrowser_render::RenderDocument;

use super::api;
use super::types::{CtxKind, JsContextAttrs, MAX_DIM};

fn type_err<S: Into<String>>(msg: S) -> JsError {
    JsNativeError::typ().with_message(msg.into()).into()
}

fn parse_dim(v: Option<String>) -> Option<u32> {
    v?.trim().parse::<u32>().ok().filter(|n| *n > 0)
}

/// Reads the canvas size: registry override → DOM attributes → 300×150
/// defaults (HTML spec).
fn canvas_size(render_doc: &Rc<RefCell<Option<RenderDocument>>>, node_id: usize) -> (u32, u32) {
    if let Some(dims) = super::canvas_dims(node_id) {
        return dims;
    }
    let doc = render_doc.borrow();
    let read = |name: &str| doc.as_ref().and_then(|d| d.node_attr(node_id, name));
    (
        parse_dim(read("width")).unwrap_or(300),
        parse_dim(read("height")).unwrap_or(150),
    )
}

/// Attaches the WebGL-aware canvas surface to a built element object.
pub fn attach(
    ctx: &mut Context,
    obj: &JsObject,
    node_id: usize,
    render_doc: Rc<RefCell<Option<RenderDocument>>>,
) {
    let _ = obj.set(
        js_string!("__oxiWebglNative"),
        JsValue::from(true),
        false,
        ctx,
    );

    // ── getContext(type, attrs?) ──
    let rd = render_doc.clone();
    let get_ctx = unsafe {
        NativeFunction::from_closure(move |this: &JsValue, args, ctx| -> JsResult<JsValue> {
            let kind = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();

            let Some(kind_enum) = CtxKind::from_type_str(&kind) else {
                // 2d and unknown kinds stay on the JS bootstrap shim.
                return call_js_2d_fallback(this, args, ctx);
            };

            // Parse the optional context-attributes dictionary.
            let attrs = match args.get_or_undefined(1) {
                v if v.is_undefined() || v.is_null() => None,
                v => {
                    let dict = v
                        .as_object()
                        .ok_or_else(|| type_err("contextAttributes must be an object"))?;
                    let mut fields: Vec<(String, bool, Option<f64>, Option<String>)> = Vec::new();
                    for (name, is_bool) in [
                        ("alpha", true),
                        ("depth", true),
                        ("stencil", true),
                        ("antialias", true),
                        ("premultipliedAlpha", true),
                        ("preserveDrawingBuffer", true),
                        ("failIfMajorPerformanceCaveat", true),
                        ("desynchronized", true),
                        ("powerPreference", false),
                    ] {
                        let val = dict
                            .get(js_string!(name), ctx)
                            .unwrap_or(JsValue::undefined());
                        if val.is_undefined() {
                            continue;
                        }
                        if is_bool {
                            fields.push((
                                name.to_string(),
                                true,
                                Some(val.to_boolean() as u32 as f64),
                                None,
                            ));
                        } else {
                            let s = val.as_string().map(|s| s.to_std_string_escaped());
                            fields.push((name.to_string(), false, None, s));
                        }
                    }
                    let src = super::types::JsAttrsSource::new(&fields);
                    Some(JsContextAttrs::from_js(&src).map_err(type_err)?)
                }
            };

            let identity = crate::js::runtime::current_webgl_identity();
            let identity_for_builder = identity.clone();
            let (w, h) = canvas_size(&rd, node_id);
            Ok(super::get_context(
                node_id,
                &kind,
                attrs,
                identity,
                w.min(MAX_DIM),
                h.min(MAX_DIM),
                move |handle, canvas| {
                    api::build_context_object(ctx, handle, kind_enum, canvas, identity_for_builder)
                },
                this.clone(),
            ))
        })
    };
    let _ = obj.set(
        js_string!("getContext"),
        JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), get_ctx)
                .name(js_string!("getContext"))
                .length(1)
                .build(),
        ),
        false,
        ctx,
    );

    // ── toDataURL ──
    let to_data_url = unsafe {
        NativeFunction::from_closure(move |_this: &JsValue, _args, _ctx| -> JsResult<JsValue> {
            let out = super::readback_straight(node_id, false)
                .and_then(|(w, h, rgba)| oxibrowser_render::encode_png(&rgba, w, h).ok())
                .map(|png| {
                    format!(
                        "data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(png)
                    )
                })
                .unwrap_or_else(|| "data:,".to_string());
            Ok(JsValue::from(js_string!(out)))
        })
    };
    let _ = obj.set(
        js_string!("toDataURL"),
        JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), to_data_url)
                .name(js_string!("toDataURL"))
                .build(),
        ),
        false,
        ctx,
    );

    // ── toBlob(cb, type?) ── PNG only (WebGL canvas spec allows image/png).
    let to_blob = unsafe {
        NativeFunction::from_closure(
            move |_this: &JsValue, args, ctx: &mut Context| -> JsResult<JsValue> {
                let cb = args
                    .first()
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| type_err("toBlob requires a callback"))?;
                let png = super::readback_straight(node_id, false)
                    .and_then(|(w, h, rgba)| oxibrowser_render::encode_png(&rgba, w, h).ok());
                // Build the Blob through the page's own Blob constructor so the
                // instanceof/blob-URL surface matches the page realm.
                let blob_ctor = ctx
                    .global_object()
                    .get(js_string!("Blob"), ctx)
                    .ok()
                    .and_then(|v| v.as_object());
                let blob: JsValue = match (png, blob_ctor) {
                    (Some(bytes), Some(ctor)) => {
                        let ab =
                            boa_engine::object::builtins::JsArrayBuffer::new(bytes.len(), ctx)?;
                        if let Some(mut data) = ab.data_mut() {
                            data.copy_from_slice(&bytes);
                        }
                        let parts = boa_engine::object::builtins::JsArray::from_iter(
                            [JsValue::from(ab)],
                            ctx,
                        );
                        let opts = {
                            let o = JsObject::with_object_proto(ctx.intrinsics());
                            o.set(
                                js_string!("type"),
                                JsValue::from(js_string!("image/png")),
                                false,
                                ctx,
                            )?;
                            o
                        };
                        let blob_obj = ctor.construct(
                            &[JsValue::from(parts), JsValue::from(opts)],
                            None,
                            ctx,
                        )?;
                        JsValue::from(blob_obj)
                    }
                    _ => JsValue::undefined(),
                };
                cb.call(&JsValue::undefined(), &[blob], ctx)?;
                Ok(JsValue::undefined())
            },
        )
    };
    let _ = obj.set(
        js_string!("toBlob"),
        JsValue::from(
            boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), to_blob)
                .name(js_string!("toBlob"))
                .length(1)
                .build(),
        ),
        false,
        ctx,
    );

    // ── width / height reflection ──
    let rd_w = render_doc.clone();
    let rd_h = render_doc.clone();
    let w_get = unsafe {
        NativeFunction::from_closure(
            move |_t: &JsValue, _a: &[JsValue], _ctx: &mut Context| -> JsResult<JsValue> {
                Ok(JsValue::from(canvas_size(&rd_w, node_id).0 as f64))
            },
        )
    };
    let h_get = unsafe {
        NativeFunction::from_closure(
            move |_t: &JsValue, _a: &[JsValue], _ctx: &mut Context| -> JsResult<JsValue> {
                Ok(JsValue::from(canvas_size(&rd_h, node_id).1 as f64))
            },
        )
    };
    let rd_ws = render_doc.clone();
    let rd_hs = render_doc.clone();
    let w_set = unsafe {
        NativeFunction::from_closure(
            move |_t: &JsValue, args, ctx: &mut Context| -> JsResult<JsValue> {
                set_attr_dim(
                    &rd_ws,
                    ctx,
                    node_id,
                    true,
                    args.first().cloned().unwrap_or_default(),
                )
            },
        )
    };
    let h_set = unsafe {
        NativeFunction::from_closure(
            move |_t: &JsValue, args, ctx: &mut Context| -> JsResult<JsValue> {
                set_attr_dim(
                    &rd_hs,
                    ctx,
                    node_id,
                    false,
                    args.first().cloned().unwrap_or_default(),
                )
            },
        )
    };
    let get_w_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), w_get)
        .name(js_string!("get width"))
        .build();
    let set_w_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), w_set)
        .name(js_string!("set width"))
        .build();
    let get_h_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), h_get)
        .name(js_string!("get height"))
        .build();
    let set_h_fn = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), h_set)
        .name(js_string!("set height"))
        .build();
    let width_key: PropertyKey = js_string!("width").into();
    let _ = obj.define_property_or_throw(
        width_key,
        PropertyDescriptor::builder()
            .get(JsValue::from(get_w_fn))
            .set(JsValue::from(set_w_fn))
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    let height_key: PropertyKey = js_string!("height").into();
    let _ = obj.define_property_or_throw(
        height_key,
        PropertyDescriptor::builder()
            .get(JsValue::from(get_h_fn))
            .set(JsValue::from(set_h_fn))
            .enumerable(true)
            .configurable(true),
        ctx,
    );
}

fn call_js_2d_fallback(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // The canvas bootstrap's `__oxi_canvas_context_2d` handles 2d and unknown
    // kinds; absent → null (no 2d implementation in this build).
    let f = ctx
        .global_object()
        .get(js_string!("__oxi_canvas_context_2d"), ctx)
        .ok()
        .and_then(|v| v.as_object());
    match f {
        Some(f) => f.call(this, args, ctx),
        None => Ok(JsValue::null()),
    }
}

/// Sets a canvas dimension (IDL write): records the size for the GL surface
/// and attempts the DOM attribute write (best-effort — blitz-dom does not
/// persist width/height on canvas nodes). Resizing clears the drawing buffer
/// per WebGL semantics.
fn set_attr_dim(
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    _ctx: &mut Context,
    node_id: usize,
    width: bool,
    value: JsValue,
) -> JsResult<JsValue> {
    let name = if width { "width" } else { "height" };
    let n = value.as_number().unwrap_or(f64::NAN);
    if !n.is_finite() || n < 0.0 {
        return Err(type_err(format!(
            "Failed to set the '{name}' property: not a non-negative number"
        )));
    }
    let v = n as u32;
    if let Some(doc) = render_doc.borrow_mut().as_mut() {
        doc.set_attribute(node_id, name, &v.to_string());
    }
    let (cur_w, cur_h) = canvas_size(render_doc, node_id);
    let (w, h) = if width { (v, cur_h) } else { (cur_w, v) };
    super::set_canvas_dims(node_id, w.min(MAX_DIM), h.min(MAX_DIM));
    super::resize(node_id, w.min(MAX_DIM), h.min(MAX_DIM));
    Ok(JsValue::from(js_string!(v.to_string())))
}
