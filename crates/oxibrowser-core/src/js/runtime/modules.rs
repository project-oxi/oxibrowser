//! URL-based ECMAScript modules for a single document/realm.
//!
//! Boa owns linking, live bindings, cycles, dynamic import and top-level await.
//! Network I/O uses the session's existing bridge, so modules inherit its
//! cookies, proxy, request overrides, offline mode and SSRF policy. Fetching
//! a module currently blocks the JS thread (like synchronous XHR), bounded by
//! the navigation deadline, without re-entering JS during network I/O.

use super::*;
use boa_engine::module::{ModuleLoader, ModuleRequest, Referrer};
use boa_engine::{JsError, Module};
use std::path::Path;
use url::Url;

pub(super) struct WebModuleLoader {
    fetch_tx: Arc<RwLock<Option<Sender<FetchRequestMsg>>>>,
    document_url: RefCell<String>,
    modules: RefCell<HashMap<String, JsResult<Module>>>,
    deadline: Cell<Option<Instant>>,
    origin_override: RefCell<Option<Option<String>>>,
}

pub(super) struct DeadlineGuard {
    loader: Rc<WebModuleLoader>,
    previous: Option<Instant>,
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        self.loader.deadline.set(self.previous);
    }
}

impl WebModuleLoader {
    pub(super) fn new(
        fetch_tx: Arc<RwLock<Option<Sender<FetchRequestMsg>>>>,
        document_url: &str,
    ) -> Self {
        Self {
            fetch_tx,
            document_url: RefCell::new(document_url.to_owned()),
            modules: RefCell::new(HashMap::new()),
            deadline: Cell::new(None),
            origin_override: RefCell::new(None),
        }
    }

    pub(super) fn set_document_url(&self, url: &str) {
        *self.document_url.borrow_mut() = url.to_owned();
    }

    /// An opaque frame cannot use the current module network subset.
    pub(super) fn set_origin_override(&self, origin: Option<String>) {
        *self.origin_override.borrow_mut() = Some(origin);
    }
    pub(super) fn script_url(&self, src: &str) -> String {
        Url::parse(&self.document_url.borrow())
            .and_then(|base| base.join(src))
            .map(|url| url.to_string())
            .unwrap_or_else(|_| src.to_owned())
    }

    pub(super) fn begin_document(self: &Rc<Self>, timeout: Duration) -> DeadlineGuard {
        self.modules.borrow_mut().clear();
        self.limit_for(timeout)
    }

    pub(super) fn limit_for(self: &Rc<Self>, timeout: Duration) -> DeadlineGuard {
        let deadline = Instant::now() + timeout;
        let previous = self.deadline.replace(Some(
            self.deadline
                .get()
                .map_or(deadline, |old| old.min(deadline)),
        ));
        DeadlineGuard {
            loader: self.clone(),
            previous,
        }
    }

    /// Script src values use document URL resolution, unlike bare import
    /// specifiers (which require an import map and are rejected below).
    pub(super) fn entry(&self, script: &ScriptSource, ctx: &mut Context) -> JsResult<Module> {
        let document_url = self.document_url.borrow().clone();
        if let Some(src) = &script.src_url {
            let url = Url::parse(&document_url)
                .and_then(|base| base.join(src))
                .map_err(|_| module_error(format!("invalid module script URL: {src}")))?;
            self.load_url(
                &url,
                (!script.source.is_empty()).then_some(script.source.as_str()),
                ctx,
            )
        } else {
            // Inline modules are separate records, even when their sources
            // match. Their base/import.meta.url is the document URL.
            parse(&script.source, &document_url, ctx)
        }
    }

    fn resolve(&self, specifier: &str, referrer: Option<&Path>) -> JsResult<Url> {
        let base = referrer
            .and_then(Path::to_str)
            .map(str::to_owned)
            .unwrap_or_else(|| self.document_url.borrow().clone());
        resolve(specifier, &base)
    }

    fn load_url(&self, url: &Url, source: Option<&str>, ctx: &mut Context) -> JsResult<Module> {
        if let Some(cached) = self.modules.borrow().get(url.as_str()).cloned() {
            return cached;
        }
        // Bound graphs as well as individual fetches. Parsed modules are
        // cached before Boa loads their dependencies, enabling cycles.
        if self.modules.borrow().len() >= 4096 {
            return Err(module_error("module graph exceeds 4096 records"));
        }
        let result = (|| {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(module_error(format!(
                    "unsupported module URL scheme: {}",
                    url.scheme()
                )));
            }
            match source {
                Some(source) => parse(source, url.as_str(), ctx),
                None => {
                    let response = self.fetch(url)?;
                    parse(&response.body, &response.url, ctx)
                }
            }
        })();
        self.modules
            .borrow_mut()
            .insert(url.to_string(), result.clone());
        result
    }

    fn fetch(&self, url: &Url) -> JsResult<FetchResponseMsg> {
        if matches!(*self.origin_override.borrow(), Some(None)) {
            return Err(module_error(
                "SecurityError: module networking is unavailable in opaque-origin frames",
            ));
        }
        let tx = self
            .fetch_tx
            .read()
            .as_ref()
            .cloned()
            .ok_or_else(|| module_error("module network bridge is unavailable"))?;
        let deadline = self
            .deadline
            .get()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        if deadline <= Instant::now() {
            return Err(module_error("module loading deadline exceeded"));
        }
        let id = next_fetch_id();
        // Use this realm's document origin, never the last-pumped frame's.
        let origin = self.origin_override.borrow().clone().unwrap_or_else(|| {
            Url::parse(&self.document_url.borrow())
                .ok()
                .map(|url| url.origin().ascii_serialization())
        });
        let headers = vec![(
            "accept".to_owned(),
            "text/javascript, application/javascript".to_owned(),
        )];
        push_event(CoreEvent::FetchRequest {
            request_id: cdp_request_id(id),
            url: url.to_string(),
            method: "GET".into(),
            headers: headers.clone(),
            post_data: None,
            timestamp: now_ms(),
        });
        tx.send(FetchRequestMsg {
            id,
            url: url.to_string(),
            method: "GET".into(),
            headers,
            body: None,
            origin: origin.clone(),
        })
        .map_err(|_| module_error("module network bridge is closed"))?;

        let response = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(module_error(format!("module loading timed out: {url}")));
            }
            let response = RESPONSE_RX
                .with(|cell| cell.borrow().as_ref().map(|rx| rx.recv_timeout(remaining)));
            let response = response
                .ok_or_else(|| module_error("module response channel is unavailable"))?
                .map_err(|_| {
                    module_error(format!("module loading timed out or disconnected: {url}"))
                })?;
            if response.id == id {
                break response;
            }
            // Preserve outstanding fetch/XHR and other frames' responses.
            // Do not pump JS here: doing so would re-enter Boa's job executor.
            DEFERRED_RESPONSES.with(|pending| pending.borrow_mut().push(response));
        };
        push_event(CoreEvent::FetchResponse {
            request_id: cdp_request_id(id),
            url: response.url.clone(),
            status: response.status,
            mime_type: content_type_mime(&response.headers),
            timestamp: now_ms(),
        });
        push_event(CoreEvent::FetchLoadingFinished {
            request_id: cdp_request_id(id),
            timestamp: now_ms(),
        });
        if let Some(error) = &response.error {
            return Err(module_error(format!(
                "module fetch failed ({url}): {error}"
            )));
        }
        if !(200..300).contains(&response.status) {
            return Err(module_error(format!(
                "module fetch failed ({url}): HTTP {}",
                response.status
            )));
        }
        let mime = content_type_mime(&response.headers);
        if !is_javascript_mime(&mime) {
            return Err(module_error(format!(
                "module requires a JavaScript MIME type ({url}), got {mime:?}"
            )));
        }
        // The shared bridge carries credentials. Until anonymous credential
        // mode is implemented, require explicit credentialed CORS approval
        // rather than accepting a wildcard and exposing a private response.
        let final_url =
            Url::parse(&response.url).map_err(|_| module_error("invalid module response URL"))?;
        if origin.as_deref() != Some(&final_url.origin().ascii_serialization()) {
            let header = |name: &str| {
                response
                    .headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim())
            };
            if header("access-control-allow-origin") != origin.as_deref()
                || header("access-control-allow-credentials") != Some("true")
            {
                return Err(module_error(format!(
                    "cross-origin module requires credentialed CORS approval: {url}"
                )));
            }
        }
        Ok(response)
    }
}

impl ModuleLoader for WebModuleLoader {
    fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> impl Future<Output = JsResult<Module>> {
        let result = (|| {
            if !request.attributes().is_empty() {
                return Err(module_error("module import attributes are not supported"));
            }
            let url = self.resolve(
                &request.specifier().to_std_string_escaped(),
                referrer.path(),
            )?;
            self.load_url(&url, None, &mut context.borrow_mut())
        })();
        std::future::ready(result)
    }

    fn init_import_meta(
        self: Rc<Self>,
        import_meta: &JsObject,
        module: &Module,
        ctx: &mut Context,
    ) {
        let url = module.path().and_then(Path::to_str).unwrap_or_default();
        let _ =
            import_meta.create_data_property_or_throw(js_string!("url"), JsString::from(url), ctx);
        let resolve_fn = NativeFunction::from_copy_closure_with_captures(
            |_, args, base: &JsString, ctx| {
                let specifier = args
                    .first()
                    .unwrap_or(&JsValue::undefined())
                    .to_string(ctx)?
                    .to_std_string_escaped();
                Ok(
                    JsString::from(resolve(&specifier, &base.to_std_string_escaped())?.as_str())
                        .into(),
                )
            },
            JsString::from(url),
        )
        .to_js_function(ctx.realm());
        let _ = import_meta.create_data_property_or_throw(js_string!("resolve"), resolve_fn, ctx);
    }
}

fn module_error(message: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

fn parse(source: &str, url: &str, ctx: &mut Context) -> JsResult<Module> {
    if source_nesting_exceeds(source) {
        return Err(JsNativeError::syntax()
            .with_message("module nesting depth exceeds the engine limit")
            .into());
    }
    Module::parse(
        Source::from_bytes(source).with_path(Path::new(url)),
        None,
        ctx,
    )
}

fn resolve(specifier: &str, base: &str) -> JsResult<Url> {
    if let Ok(url) = Url::parse(specifier) {
        return Ok(url);
    }
    if !(specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../"))
    {
        return Err(module_error(format!(
            "unmapped bare module specifier: {specifier}"
        )));
    }
    Url::parse(base)
        .and_then(|base| base.join(specifier))
        .map_err(|_| module_error(format!("cannot resolve module {specifier:?} from {base:?}")))
}

fn is_javascript_mime(mime: &str) -> bool {
    matches!(
        mime.to_ascii_lowercase().as_str(),
        "text/javascript"
            | "application/javascript"
            | "text/ecmascript"
            | "application/ecmascript"
            | "application/x-javascript"
            | "application/x-ecmascript"
            | "text/jscript"
            | "text/livescript"
            | "text/x-javascript"
            | "text/x-ecmascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
    )
}
