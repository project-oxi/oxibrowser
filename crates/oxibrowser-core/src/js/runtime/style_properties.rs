//! Live inline style declarations, backed by the renderer's CSS parser.

use super::*;

pub(super) fn register(ctx: &mut Context) {
    // Only property-name mapping belongs here. Parsing, declaration order,
    // priorities, and serialization remain the renderer's responsibility.
    const SOURCE: &str = r#"
    globalThis.__oxiMakeStyle = function (target) {
      var read = target.getPropertyValue;
      var write = target.setProperty;
      function property(key) {
        if (key === 'cssFloat') return 'float';
        if (key.slice(0, 2) === '--') return key;
        return key.replace(/[A-Z]/g, function (c) { return '-' + c.toLowerCase(); })
                  .replace(/^ms-/, '-ms-');
      }
      return new Proxy(target, {
        get: function (_, key, receiver) {
          if (typeof key !== 'string' || Reflect.has(target, key)) {
            return Reflect.get(target, key, receiver);
          }
          return read.call(target, property(key));
        },
        set: function (_, key, value, receiver) {
          if (typeof key !== 'string' || Reflect.has(target, key)) {
            return Reflect.set(target, key, value, receiver);
          }
          write.call(target, property(key), value);
          return true;
        }
      });
    };
    "#;
    if let Err(error) = ctx.eval(Source::from_bytes(SOURCE)) {
        tracing::warn!(%error, "Inline style properties bootstrap failed");
    }
}

fn require_arguments(args: &[JsValue], count: usize, method: &str) -> JsResult<()> {
    if args.len() < count {
        return Err(JsNativeError::typ()
            .with_message(format!("{method} requires {count} argument(s)"))
            .into());
    }
    Ok(())
}

fn string(value: &JsValue, ctx: &mut Context) -> JsResult<String> {
    Ok(value.to_string(ctx)?.to_std_string_escaped())
}

// CSSOM's value and priority arguments use LegacyNullToEmptyString.
fn nullable_string(value: &JsValue, ctx: &mut Context) -> JsResult<String> {
    if value.is_null() {
        Ok(String::new())
    } else {
        string(value, ctx)
    }
}

pub(super) fn create(
    ctx: &mut Context,
    render_doc: Rc<RefCell<Option<RenderDocument>>>,
    node_id: usize,
    dom_dirty: Arc<AtomicBool>,
) -> JsResult<JsValue> {
    // SAFETY: native closures below capture only Rust-owned renderer state and
    // primitive node IDs, never JavaScript objects requiring GC tracing. All
    // argument conversions finish before a renderer borrow can be acquired;
    // conversion may call user JavaScript that reads or changes this style.
    let rd = render_doc.clone();
    let get_value = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            require_arguments(args, 1, "getPropertyValue")?;
            let property = string(&args[0], ctx)?;
            let value = rd
                .borrow()
                .as_ref()
                .map(|doc| doc.inline_style(node_id, &property).0)
                .unwrap_or_default();
            Ok(JsString::from(value.as_str()).into())
        })
    };
    let rd = render_doc.clone();
    let get_priority = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            require_arguments(args, 1, "getPropertyPriority")?;
            let property = string(&args[0], ctx)?;
            let important = rd
                .borrow()
                .as_ref()
                .is_some_and(|doc| doc.inline_style(node_id, &property).1);
            Ok(JsString::from(if important { "important" } else { "" }).into())
        })
    };
    let rd = render_doc.clone();
    let dirty = dom_dirty.clone();
    let set_property = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            require_arguments(args, 2, "setProperty")?;
            let property = string(&args[0], ctx)?;
            let value = nullable_string(&args[1], ctx)?;
            let priority = match args.get(2) {
                Some(value) if !value.is_undefined() => nullable_string(value, ctx)?,
                _ => String::new(),
            };
            // An empty value removes a declaration even if priority is invalid.
            // Otherwise CSSOM accepts only an empty priority or 'important'.
            let important = priority.eq_ignore_ascii_case("important");
            if !value.is_empty() && !priority.is_empty() && !important {
                return Ok(JsValue::undefined());
            }
            if let Some(doc) = rd.borrow_mut().as_mut() {
                if value.is_empty() {
                    doc.remove_inline_style(node_id, &property);
                } else {
                    doc.set_inline_style_with_priority(node_id, &property, &value, important);
                }
                dirty.store(true, Ordering::Relaxed);
            }
            Ok(JsValue::undefined())
        })
    };
    let rd = render_doc.clone();
    let dirty = dom_dirty.clone();
    let remove_property = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            require_arguments(args, 1, "removeProperty")?;
            let property = string(&args[0], ctx)?;
            let previous = if let Some(doc) = rd.borrow_mut().as_mut() {
                let previous = doc.remove_inline_style(node_id, &property);
                dirty.store(true, Ordering::Relaxed);
                previous
            } else {
                String::new()
            };
            Ok(JsString::from(previous.as_str()).into())
        })
    };
    let target = boa_engine::object::ObjectInitializer::new(ctx)
        .function(get_value, js_string!("getPropertyValue"), 1)
        .function(get_priority, js_string!("getPropertyPriority"), 1)
        .function(set_property, js_string!("setProperty"), 2)
        .function(remove_property, js_string!("removeProperty"), 1)
        .build();

    let rd = render_doc.clone();
    let get_text = unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            let value = rd
                .borrow()
                .as_ref()
                .and_then(|doc| doc.node_attr(node_id, "style"))
                .unwrap_or_default();
            Ok(JsString::from(value.as_str()).into())
        })
    }
    .to_js_function(ctx.realm());
    let set_text = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let value = nullable_string(args.first().unwrap_or(&JsValue::undefined()), ctx)?;
            if let Some(doc) = render_doc.borrow_mut().as_mut() {
                doc.set_attribute(node_id, "style", &value);
                dom_dirty.store(true, Ordering::Relaxed);
            }
            Ok(JsValue::undefined())
        })
    }
    .to_js_function(ctx.realm());
    target.define_property_or_throw(
        js_string!("cssText"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(get_text)
            .set(set_text)
            .enumerable(true)
            .configurable(true),
        ctx,
    )?;

    let factory = ctx.global_object().get(js_string!("__oxiMakeStyle"), ctx)?;
    let factory = factory
        .as_callable()
        .ok_or_else(|| JsNativeError::typ().with_message("Inline style factory is unavailable"))?;
    factory.call(&JsValue::undefined(), &[target.into()], ctx)
}
