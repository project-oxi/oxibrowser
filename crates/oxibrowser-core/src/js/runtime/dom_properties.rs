//! Live DOM attribute properties used by module-based applications.

use super::*;

pub(super) fn register(ctx: &mut Context) {
    const SOURCE: &str = r#"
    globalThis.__oxiMakeDataset = function (element) {
      function attribute(key) {
        return 'data-' + key.replace(/[A-Z]/g, function (c) { return '-' + c.toLowerCase(); });
      }
      function read(key) { return element.getAttribute(attribute(key)); }
      var target = {};
      return new Proxy(target, {
        get: function (_, key) {
          if (typeof key !== 'string') return Reflect.get(target, key);
          var value = read(key);
          return value === null ? Reflect.get(target, key) : value;
        },
        set: function (_, key, value) {
          if (typeof key !== 'string') return Reflect.set(target, key, value);
          if (/-[a-z]/.test(key)) throw new SyntaxError('Invalid dataset property name');
          element.setAttribute(attribute(key), String(value));
          return true;
        },
        deleteProperty: function (_, key) {
          if (typeof key !== 'string') return Reflect.deleteProperty(target, key);
          element.removeAttribute(attribute(key));
          return true;
        },
        has: function (_, key) {
          return (typeof key === 'string' && read(key) !== null) || Reflect.has(target, key);
        },
        ownKeys: function () {
          return element.getAttributeNames().filter(function (name) {
            return name.slice(0, 5) === 'data-' && !/[A-Z]/.test(name.slice(5));
          }).map(function (name) {
            return name.slice(5).replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
          }).concat(Object.getOwnPropertySymbols(target));
        },
        getOwnPropertyDescriptor: function (_, key) {
          if (typeof key !== 'string') return Reflect.getOwnPropertyDescriptor(target, key);
          var value = read(key);
          return value === null ? undefined : { value: value, enumerable: true, configurable: true, writable: true };
        }
      });
    };
    "#;
    if let Err(error) = ctx.eval(Source::from_bytes(SOURCE)) {
        tracing::warn!(%error, "DOM attribute properties bootstrap failed");
    }
}

pub(super) fn attach(
    ctx: &mut Context,
    object: &JsObject,
    render_doc: Rc<RefCell<Option<RenderDocument>>>,
    node_id: usize,
    dirty: Arc<AtomicBool>,
) {
    let rd = render_doc.clone();
    let names = unsafe {
        NativeFunction::from_closure(move |_, _, ctx| {
            let names = rd
                .borrow()
                .as_ref()
                .map(|doc| doc.node_attributes(node_id))
                .unwrap_or_default();
            let array = JsArray::from_iter(
                names
                    .into_iter()
                    .map(|(name, _)| JsValue::from(JsString::from(name.as_str()))),
                ctx,
            );
            Ok(array.into())
        })
    }
    .to_js_function(ctx.realm());
    let _ = object.create_data_property_or_throw(js_string!("getAttributeNames"), names, ctx);
    for (name, attribute, boolean) in [
        ("hidden", "hidden", true),
        ("inert", "inert", true),
        ("disabled", "disabled", true),
        ("title", "title", false),
        ("lang", "lang", false),
        ("name", "name", false),
        ("rel", "rel", false),
        ("crossOrigin", "crossorigin", false),
        ("as", "as", false),
    ] {
        let rd = render_doc.clone();
        let get = unsafe {
            NativeFunction::from_closure(move |_, _, _| {
                let value = rd
                    .borrow()
                    .as_ref()
                    .and_then(|doc| doc.node_attr(node_id, attribute));
                Ok(if boolean {
                    value.is_some().into()
                } else {
                    JsString::from(value.unwrap_or_default().as_str()).into()
                })
            })
        }
        .to_js_function(ctx.realm());
        let rd = render_doc.clone();
        let dirty = dirty.clone();
        let set = unsafe {
            NativeFunction::from_closure(move |_, args, ctx| {
                let input = args.first().cloned().unwrap_or_default();
                let value = if boolean {
                    input.to_boolean().then(String::new)
                } else {
                    Some(input.to_string(ctx)?.to_std_string_escaped())
                };
                if let Some(doc) = rd.borrow_mut().as_mut() {
                    if let Some(value) = value {
                        doc.set_attribute(node_id, attribute, &value);
                    } else {
                        doc.remove_attribute(node_id, attribute);
                    }
                    dirty.store(true, Ordering::Relaxed);
                }
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(get)
                .set(set)
                .configurable(true),
            ctx,
        );
    }
    for (name, axis) in [
        ("clientWidth", 0),
        ("clientHeight", 1),
        ("offsetWidth", 0),
        ("offsetHeight", 1),
    ] {
        let rd = render_doc.clone();
        let get = unsafe {
            NativeFunction::from_closure(move |_, _, _| {
                let rect = rd
                    .borrow_mut()
                    .as_mut()
                    .map(|doc| {
                        doc.flush_layout();
                        doc.node_layout_rect(node_id)
                    })
                    .unwrap_or_default();
                Ok(JsValue::from(if axis == 0 { rect.2 } else { rect.3 }))
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(get)
                .configurable(true),
            ctx,
        );
    }
    let getter = NativeFunction::from_fn_ptr(|this, _, ctx| {
        let object = this
            .as_object()
            .ok_or_else(|| JsNativeError::typ().with_message("dataset requires an element"))?;
        let cached = object.get(js_string!("__oxiDataset"), ctx)?;
        if cached.is_object() {
            return Ok(cached);
        }
        let factory = ctx
            .global_object()
            .get(js_string!("__oxiMakeDataset"), ctx)?;
        let factory = factory
            .as_callable()
            .ok_or_else(|| JsNativeError::typ().with_message("dataset factory is unavailable"))?;
        let dataset = factory.call(&JsValue::undefined(), std::slice::from_ref(this), ctx)?;
        object.define_property_or_throw(
            js_string!("__oxiDataset"),
            boa_engine::property::PropertyDescriptor::builder()
                .value(dataset.clone())
                .configurable(true),
            ctx,
        )?;
        Ok(dataset)
    })
    .to_js_function(ctx.realm());
    let _ = object.define_property_or_throw(
        js_string!("dataset"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(getter)
            .enumerable(true)
            .configurable(true),
        ctx,
    );
}
