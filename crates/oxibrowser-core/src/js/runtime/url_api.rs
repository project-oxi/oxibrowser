//! Rust URL parsing and live, ordered form-query bindings.
use super::*;
use boa_engine::JsSymbol;
use boa_engine::object::{ConstructorBuilder, ObjectInitializer};
use url::{Url, form_urlencoded, quirks};

type Pairs = Vec<(String, String)>;

#[derive(Clone)]
enum Params {
    Url(Arc<RwLock<Url>>),
    Standalone(Arc<RwLock<Pairs>>),
}

impl Params {
    fn pairs(&self) -> Pairs {
        match self {
            Self::Url(url) => decode(url.read().query().unwrap_or_default()),
            Self::Standalone(pairs) => pairs.read().clone(),
        }
    }

    // No JavaScript may run while holding either lock.
    fn update(&self, edit: impl FnOnce(&mut Pairs)) {
        match self {
            Self::Url(url) => {
                let mut url = url.write();
                let mut pairs = decode(url.query().unwrap_or_default());
                edit(&mut pairs);
                let query = encode(&pairs);
                url.set_query((!query.is_empty()).then_some(query.as_str()));
            }
            Self::Standalone(pairs) => edit(&mut pairs.write()),
        }
    }
}

fn decode(query: &str) -> Pairs {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

fn encode(pairs: &Pairs) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(key, value)| (key, value)))
        .finish()
}

fn string_arg(args: &[JsValue], index: usize, ctx: &mut Context) -> JsResult<String> {
    args.get(index)
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)
        .map(|value| value.to_std_string_lossy())
}

fn js(value: impl AsRef<str>) -> JsValue {
    JsString::from(value.as_ref()).into()
}

fn prototype(object: &JsObject, name: &str, ctx: &mut Context) -> JsResult<()> {
    let constructor = ctx.global_object().get(JsString::from(name), ctx)?;
    if let Some(constructor) = constructor.as_object() {
        let proto = constructor.get(js_string!("prototype"), ctx)?;
        if let Some(proto) = proto.as_object() {
            object.set_prototype(Some(proto.clone()));
        }
    }
    Ok(())
}

/// Register globals before attaching the existing blob URL static methods.
pub(super) fn register(ctx: &mut Context) -> JsResult<()> {
    let mut params = ConstructorBuilder::new(ctx, NativeFunction::from_fn_ptr(params_constructor));
    params.name("URLSearchParams").length(0);
    let params = params.build();
    ctx.register_global_property(
        js_string!("URLSearchParams"),
        params.constructor(),
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    )?;
    let mut url = ConstructorBuilder::new(ctx, NativeFunction::from_fn_ptr(url_constructor));
    url.name("URL").length(1);
    let url = url.build();
    ctx.register_global_property(
        js_string!("URL"),
        url.constructor(),
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    )?;
    Ok(())
}

fn url_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let input = string_arg(args, 0, ctx)?;
    let base = if args.get(1).is_some_and(|value| !value.is_undefined()) {
        Some(
            Url::parse(&string_arg(args, 1, ctx)?)
                .map_err(|_| JsNativeError::typ().with_message("Invalid base URL"))?,
        )
    } else {
        None
    };
    let parsed = Url::options()
        .base_url(base.as_ref())
        .parse(&input)
        .map_err(|_| JsNativeError::typ().with_message("Invalid URL"))?;
    let state = Arc::new(RwLock::new(parsed));
    let params = params_object(Params::Url(state.clone()), ctx)?;
    let object = ObjectInitializer::new(ctx)
        .property(js_string!("searchParams"), params, Attribute::ENUMERABLE)
        .build();
    prototype(&object, "URL", ctx)?;
    for field in [
        "href", "origin", "protocol", "host", "hostname", "pathname", "search", "hash", "port",
        "username", "password",
    ] {
        let read = state.clone();
        // SAFETY: captured values contain Rust data only, never GC-managed values.
        let getter = unsafe {
            NativeFunction::from_closure(move |_, _, _| {
                let url = read.read();
                Ok(js(match field {
                    "href" => url.as_str().to_owned(),
                    "origin" => quirks::origin(&url),
                    "protocol" => quirks::protocol(&url).to_owned(),
                    "host" => quirks::host(&url).to_owned(),
                    "hostname" => quirks::hostname(&url).to_owned(),
                    "pathname" => quirks::pathname(&url).to_owned(),
                    "search" => quirks::search(&url).to_owned(),
                    "hash" => quirks::hash(&url).to_owned(),
                    "port" => quirks::port(&url).to_owned(),
                    "username" => quirks::username(&url).to_owned(),
                    "password" => quirks::password(&url).to_owned(),
                    _ => unreachable!(),
                }))
            })
        };
        let getter = FunctionObjectBuilder::new(ctx.realm(), getter).build();
        let setter = if field == "origin" {
            None
        } else {
            let write = state.clone();
            // SAFETY: only the Rust URL state and a static field name are captured.
            let setter = unsafe {
                NativeFunction::from_closure(move |_, args, ctx| {
                    let value = string_arg(args, 0, ctx)?;
                    let mut url = write.write();
                    match field {
                        "href" => quirks::set_href(&mut url, &value)
                            .map_err(|_| JsNativeError::typ().with_message("Invalid URL"))?,
                        "protocol" => {
                            let _ = quirks::set_protocol(&mut url, &value);
                        }
                        "host" => {
                            let _ = quirks::set_host(&mut url, &value);
                        }
                        "hostname" => {
                            let _ = quirks::set_hostname(&mut url, &value);
                        }
                        "pathname" => quirks::set_pathname(&mut url, &value),
                        "search" => quirks::set_search(&mut url, &value),
                        "hash" => quirks::set_hash(&mut url, &value),
                        "port" => {
                            let _ = quirks::set_port(&mut url, &value);
                        }
                        "username" => {
                            let _ = quirks::set_username(&mut url, &value);
                        }
                        "password" => {
                            let _ = quirks::set_password(&mut url, &value);
                        }
                        _ => unreachable!(),
                    }
                    Ok(JsValue::undefined())
                })
            };
            Some(FunctionObjectBuilder::new(ctx.realm(), setter).build())
        };
        object.define_property_or_throw(
            JsString::from(field),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .maybe_set(setter)
                .enumerable(true)
                .configurable(true),
            ctx,
        )?;
    }
    for name in ["toString", "toJSON"] {
        let read = state.clone();
        // SAFETY: the closure captures Rust-only state.
        let function =
            unsafe { NativeFunction::from_closure(move |_, _, _| Ok(js(read.read().as_str()))) };
        let function = FunctionObjectBuilder::new(ctx.realm(), function)
            .name(name)
            .build();
        object.create_data_property_or_throw(JsString::from(name), function, ctx)?;
    }
    Ok(object.into())
}

// Array.from implements iteration and IteratorClose, including nested pair iterables.
fn iterable_array(value: &JsValue, ctx: &mut Context) -> JsResult<JsArray> {
    let array = ctx.intrinsics().constructors().array().constructor();
    let from = array.get(js_string!("from"), ctx)?;
    let from = from
        .as_callable()
        .ok_or_else(|| JsNativeError::typ().with_message("Array.from is not callable"))?;
    let result = from.call(&array.into(), std::slice::from_ref(value), ctx)?;
    JsArray::from_object(
        result
            .as_object()
            .ok_or_else(|| JsNativeError::typ().with_message("Expected array"))?,
    )
}

fn params_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let init = args.first().cloned().unwrap_or_default();
    let pairs = if init.is_null_or_undefined() {
        vec![]
    } else if let Some(object) = init.as_object() {
        let mut pairs = vec![];
        if !object
            .get(JsSymbol::iterator(), ctx)?
            .is_null_or_undefined()
        {
            let outer = iterable_array(&init, ctx)?;
            for index in 0..outer.length(ctx)? {
                let pair = outer.get(index, ctx)?;
                let object = pair.as_object().ok_or_else(|| {
                    JsNativeError::typ().with_message("Query pair must be iterable")
                })?;
                if object
                    .get(JsSymbol::iterator(), ctx)?
                    .is_null_or_undefined()
                {
                    return Err(JsNativeError::typ()
                        .with_message("Query pair must be iterable")
                        .into());
                }
                let pair = iterable_array(&pair, ctx)?;
                if pair.length(ctx)? != 2 {
                    return Err(JsNativeError::typ()
                        .with_message("Query pair must have two items")
                        .into());
                }
                let key = pair.get(0, ctx)?.to_string(ctx)?.to_std_string_lossy();
                let value = pair.get(1, ctx)?.to_string(ctx)?.to_std_string_lossy();
                pairs.push((key, value));
            }
        } else {
            let keys = boa_engine::builtins::object::OrdinaryObject::keys(
                &JsValue::undefined(),
                std::slice::from_ref(&init),
                ctx,
            )?;
            let keys =
                JsArray::from_object(keys.as_object().expect("Object.keys returns an array"))?;
            for index in 0..keys.length(ctx)? {
                let key = keys.get(index, ctx)?.to_string(ctx)?;
                let value = object
                    .get(key.clone(), ctx)?
                    .to_string(ctx)?
                    .to_std_string_lossy();
                pairs.push((key.to_std_string_lossy(), value));
            }
        }
        pairs
    } else {
        let query = init.to_string(ctx)?.to_std_string_lossy();
        decode(query.strip_prefix('?').unwrap_or(&query))
    };
    Ok(params_object(Params::Standalone(Arc::new(RwLock::new(pairs))), ctx)?.into())
}

fn params_object(state: Params, ctx: &mut Context) -> JsResult<JsObject> {
    let object = ObjectInitializer::new(ctx).build();
    prototype(&object, "URLSearchParams", ctx)?;
    for name in [
        "get", "getAll", "has", "set", "append", "delete", "toString", "sort", "entries", "keys",
        "values", "forEach",
    ] {
        let state = state.clone();
        let length = match name {
            "set" | "append" => 2,
            "get" | "getAll" | "has" | "delete" | "forEach" => 1,
            _ => 0,
        };
        // SAFETY: Params contains only Rust URL/pair state.
        let function = unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                if args.len() < length {
                    return Err(JsNativeError::typ()
                        .with_message("Not enough arguments")
                        .into());
                }
                match name {
                    "toString" => Ok(js(encode(&state.pairs()))),
                    "entries" | "keys" | "values" => {
                        iterator(state.clone(), name, ctx).map(Into::into)
                    }
                    "forEach" => {
                        let callback =
                            args.first().and_then(JsValue::as_callable).ok_or_else(|| {
                                JsNativeError::typ().with_message("Callback must be callable")
                            })?;
                        let this_arg = args.get(1).cloned().unwrap_or_default();
                        let mut index = 0;
                        while let Some((key, value)) = state.pairs().get(index).cloned() {
                            callback.call(&this_arg, &[js(value), js(key), this.clone()], ctx)?;
                            index += 1;
                        }
                        Ok(JsValue::undefined())
                    }
                    "sort" => {
                        state.update(|pairs| {
                            pairs.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()))
                        });
                        Ok(JsValue::undefined())
                    }
                    _ => {
                        let key = string_arg(args, 0, ctx)?;
                        let value = if name == "set"
                            || name == "append"
                            || ((name == "has" || name == "delete")
                                && args.get(1).is_some_and(|v| !v.is_undefined()))
                        {
                            Some(string_arg(args, 1, ctx)?)
                        } else {
                            None
                        };
                        match name {
                            "get" => Ok(state
                                .pairs()
                                .iter()
                                .find(|(k, _)| k == &key)
                                .map(|(_, v)| js(v))
                                .unwrap_or(JsValue::null())),
                            "getAll" => Ok(JsArray::from_iter(
                                state
                                    .pairs()
                                    .iter()
                                    .filter(|(k, _)| k == &key)
                                    .map(|(_, v)| js(v)),
                                ctx,
                            )
                            .into()),
                            "has" => Ok(state
                                .pairs()
                                .iter()
                                .any(|(k, v)| {
                                    k == &key && value.as_ref().is_none_or(|value| v == value)
                                })
                                .into()),
                            "delete" => {
                                state.update(|pairs| {
                                    pairs.retain(|(k, v)| {
                                        k != &key || value.as_ref().is_some_and(|value| v != value)
                                    })
                                });
                                Ok(JsValue::undefined())
                            }
                            "append" => {
                                state.update(|pairs| pairs.push((key, value.unwrap())));
                                Ok(JsValue::undefined())
                            }
                            "set" => {
                                state.update(|pairs| {
                                    let mut found = false;
                                    pairs.retain_mut(|(k, v)| {
                                        if k != &key {
                                            return true;
                                        }
                                        if found {
                                            return false;
                                        }
                                        *v = value.as_ref().unwrap().clone();
                                        found = true;
                                        true
                                    });
                                    if !found {
                                        pairs.push((key, value.unwrap()));
                                    }
                                });
                                Ok(JsValue::undefined())
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            })
        };
        let function = FunctionObjectBuilder::new(ctx.realm(), function)
            .name(name)
            .length(length)
            .build();
        object.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .value(function)
                .writable(true)
                .configurable(true),
            ctx,
        )?;
    }
    let entries = object.get(js_string!("entries"), ctx)?;
    object.create_data_property_or_throw(JsSymbol::iterator(), entries, ctx)?;
    // SAFETY: only the Rust query state is captured.
    let size = unsafe {
        NativeFunction::from_closure(move |_, _, _| Ok((state.pairs().len() as f64).into()))
    };
    let size = FunctionObjectBuilder::new(ctx.realm(), size).build();
    object.define_property_or_throw(
        js_string!("size"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(size)
            .enumerable(true)
            .configurable(true),
        ctx,
    )?;
    Ok(object)
}

fn iterator(state: Params, kind: &'static str, ctx: &mut Context) -> JsResult<JsObject> {
    let index = Arc::new(RwLock::new(Some(0usize)));
    // SAFETY: query state and iteration index are Rust-only; no GC values are captured.
    let next = unsafe {
        NativeFunction::from_closure(move |_, _, ctx| {
            let item = {
                let mut index = index.write();
                let item = index.and_then(|index| state.pairs().get(index).cloned());
                *index = match (&item, *index) {
                    (Some(_), Some(index)) => Some(index + 1),
                    _ => None,
                };
                item
            };
            let done = item.is_none();
            let value = match item {
                Some((key, value)) => match kind {
                    "keys" => js(key),
                    "values" => js(value),
                    _ => JsArray::from_iter([js(key), js(value)], ctx).into(),
                },
                None => JsValue::undefined(),
            };
            Ok(ObjectInitializer::new(ctx)
                .property(js_string!("value"), value, Attribute::all())
                .property(js_string!("done"), done, Attribute::all())
                .build()
                .into())
        })
    };
    Ok(ObjectInitializer::new(ctx)
        .function(next, js_string!("next"), 0)
        .function(
            NativeFunction::from_fn_ptr(|this, _, _| Ok(this.clone())),
            JsSymbol::iterator(),
            0,
        )
        .build())
}
