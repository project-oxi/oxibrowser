//! Resources fetched and authorized by core, never by the renderer.
//!
//! Blitz's NetHandler has no failure method. Unknown head stylesheets would
//! therefore remain critical forever if handed to its eager loader. The parser
//! adapter defers initial link loading, restores original attributes, and
//! attaches each fetched request's own stylesheet through Blitz's public API.
//! The CSS parser receives its actual final URL independently of DOM href.

use parking_lot::RwLock;
use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use blitz_dom::{BaseDocument, DocumentConfig, QualName};
use blitz_html::DocumentHtmlParser;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request, Url};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, ParseOpts};

#[derive(Clone, Debug)]
pub struct PreloadedStylesheet {
    pub request_url: String,
    pub response_url: String,
    /// Genuine response CSS, decoded by core's HTTP charset policy to UTF-8.
    pub body: Vec<u8>,
}

/// A document-scoped allowlist of actual stylesheet response bytes. Cloning
/// shares immutable data; constructing another document does not reuse it.
#[derive(Clone, Debug, Default)]
pub struct PreloadedStylesheets(Arc<BTreeMap<String, PreloadedStylesheet>>);

impl PreloadedStylesheets {
    pub fn insert(&mut self, stylesheet: PreloadedStylesheet) {
        Arc::make_mut(&mut self.0).insert(stylesheet.request_url.clone(), stylesheet);
    }

    pub fn iter(&self) -> impl Iterator<Item = &PreloadedStylesheet> {
        self.0.values()
    }

    fn get(&self, request_url: &str) -> Option<&PreloadedStylesheet> {
        self.0.get(request_url)
    }
}

struct MemoryProvider {
    stylesheets: PreloadedStylesheets,
    active: Arc<RwLock<HashSet<String>>>,
}

impl NetProvider for MemoryProvider {
    fn fetch(&self, _: usize, request: Request, handler: Box<dyn NetHandler>) {
        if let Some(sheet) = self.stylesheets.get(request.url.as_str()) {
            // Stylesheet parsing can request @imports synchronously. Stop
            // cycles among already-preloaded URLs without inventing content.
            if !self.active.write().insert(request.url.to_string()) {
                tracing::debug!(url = %request.url, "cyclic preloaded CSS import skipped");
                return;
            }
            handler.bytes(
                sheet.response_url.clone(),
                Bytes::copy_from_slice(&sheet.body),
            );
            self.active.write().remove(request.url.as_str());
        } else {
            // Fonts/images/@imports outside the supplied map are unsupported.
            // Never perform an unvetted fetch or invent a successful response.
            tracing::debug!(url = %request.url, "renderer resource not preloaded");
        }
    }
}

pub(crate) fn parse(
    html: &str,
    mut config: DocumentConfig,
    stylesheets: &PreloadedStylesheets,
) -> BaseDocument {
    let base = config
        .base_url
        .as_deref()
        .and_then(|url| Url::parse(url).ok());
    config.net_provider = Some(Arc::new(MemoryProvider {
        stylesheets: stylesheets.clone(),
        active: Arc::new(RwLock::new(HashSet::new())),
    }));
    if let Some(sheets) = &mut config.ua_stylesheets
        && !sheets.iter().any(|sheet| sheet == blitz_dom::DEFAULT_CSS)
    {
        sheets.push(blitz_dom::DEFAULT_CSS.to_string());
    }
    let restored = RefCell::new(Vec::new());
    let linked_stylesheets = RefCell::new(Vec::new());
    let mut doc = BaseDocument::new(config);
    {
        let mut mutator = doc.mutate();
        let is_xml = html.starts_with("<?xml")
            || html.starts_with("<!DOCTYPE")
                && html
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains("XHTML") || line.contains("xhtml"));
        let mut inner = DocumentHtmlParser::new(&mut mutator);
        inner.is_xml = is_xml;
        let sink = ResourceSink {
            inner,
            base,
            stylesheets,
            restored: &restored,
            linked_stylesheets: &linked_stylesheets,
        };
        let mut options = ParseOpts::default();
        options.tree_builder.scripting_enabled = false;
        options.tree_builder.drop_doctype = true;
        options.tree_builder.quirks_mode = QuirksMode::NoQuirks;
        if is_xml {
            xml5ever::driver::parse_document(sink, Default::default()).one(html);
        } else {
            html5ever::parse_document(sink, options).one(html);
        }
    }
    for (id, attrs) in restored.into_inner() {
        if let Some(element) = doc
            .get_node_mut(id)
            .and_then(|node| node.element_data_mut())
        {
            for attr in attrs {
                element.attrs.set(attr.name, &attr.value);
            }
        }
    }
    for (node, request_url) in linked_stylesheets.into_inner() {
        let Some(record) = stylesheets.get(&request_url) else {
            continue;
        };
        let (Ok(css), Ok(final_url)) = (
            std::str::from_utf8(&record.body),
            Url::parse(&record.response_url),
        ) else {
            tracing::warn!(url = %request_url, "invalid preloaded stylesheet encoding or final URL");
            continue;
        };
        let guard = doc.guard().clone();
        // Use the real CSS parser and the document's existing lock. A response
        // URL supplies CSS's base; request_url selects only this request's
        // bytes, even when several requests redirect to the same final URL.
        let stylesheet = style::stylesheets::Stylesheet::from_str(
            css,
            style::stylesheets::UrlExtraData(style::servo_arc::Arc::new(final_url)),
            style::stylesheets::Origin::Author,
            style::servo_arc::Arc::new(guard.wrap(style::media_queries::MediaList::empty())),
            guard,
            None, // New network fetching of @imports is unsupported.
            None,
            style::context::QuirksMode::NoQuirks,
            style::stylesheets::AllowImportRules::Yes,
        );
        doc.add_stylesheet_for_node(
            style::stylesheets::DocumentStyleSheet(style::servo_arc::Arc::new(stylesheet)),
            node,
        );
    }
    doc
}

struct ResourceSink<'m, 'doc, 'resource> {
    inner: DocumentHtmlParser<'m, 'doc>,
    base: Option<Url>,
    stylesheets: &'resource PreloadedStylesheets,
    restored: &'resource RefCell<Vec<(usize, Vec<Attribute>)>>,
    linked_stylesheets: &'resource RefCell<Vec<(usize, String)>>,
}

impl TreeSink for ResourceSink<'_, '_, '_> {
    type Output = ();
    type Handle = usize;
    type ElemName<'a>
        = Ref<'a, QualName>
    where
        Self: 'a;

    fn finish(self) {
        self.inner.finish();
    }
    fn parse_error(&self, msg: Cow<'static, str>) {
        self.inner.parse_error(msg);
    }
    fn get_document(&self) -> usize {
        self.inner.get_document()
    }
    fn elem_name<'a>(&'a self, node: &'a usize) -> Self::ElemName<'a> {
        self.inner.elem_name(node)
    }
    fn create_element(
        &self,
        name: QualName,
        mut attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> usize {
        let attr = |key: &str| {
            attrs
                .iter()
                .find(|attr| attr.name.local.as_ref() == key)
                .map(|attr| attr.value.as_ref())
        };
        let is_stylesheet = name.local.as_ref() == "link"
            && attr("rel").is_some_and(|rel| {
                rel.split_ascii_whitespace()
                    .any(|token| token.eq_ignore_ascii_case("stylesheet"))
            });
        let stylesheet_request = if is_stylesheet {
            attr("href")
                .and_then(|href| {
                    self.base
                        .as_ref()
                        .and_then(|base| base.join(href).ok())
                        .or_else(|| Url::parse(href).ok())
                })
                .and_then(|url| {
                    self.stylesheets
                        .get(url.as_str())
                        .map(|record| record.request_url.clone())
                })
        } else {
            None
        };
        let masked = if is_stylesheet {
            attr("href").map(|_| "href")
        } else if name.local.as_ref() == "img"
            && attr("src").is_some_and(|src| {
                self.base
                    .as_ref()
                    .and_then(|base| base.join(src).ok())
                    .or_else(|| Url::parse(src).ok())
                    .is_none()
            })
        {
            // Blitz unwraps relative image resolution during parse too. Only
            // mask URLs that cannot resolve against this document's own base.
            Some("src")
        } else {
            None
        };
        let saved: Vec<Attribute> = if let Some(masked) = masked {
            attrs
                .extract_if(.., |attr| attr.name.local.as_ref() == masked)
                .collect()
        } else {
            Vec::new()
        };
        let id = self.inner.create_element(name, attrs, flags);
        if let Some(request) = stylesheet_request {
            self.linked_stylesheets.borrow_mut().push((id, request));
        }
        if !saved.is_empty() {
            self.restored.borrow_mut().push((id, saved));
        }
        id
    }
    fn create_comment(&self, text: StrTendril) -> usize {
        self.inner.create_comment(text)
    }
    fn create_pi(&self, target: StrTendril, data: StrTendril) -> usize {
        self.inner.create_pi(target, data)
    }
    fn append(&self, parent: &usize, child: NodeOrText<usize>) {
        self.inner.append(parent, child);
    }
    fn append_based_on_parent_node(
        &self,
        element: &usize,
        previous: &usize,
        child: NodeOrText<usize>,
    ) {
        self.inner
            .append_based_on_parent_node(element, previous, child);
    }
    fn append_doctype_to_document(&self, name: StrTendril, public: StrTendril, system: StrTendril) {
        self.inner.append_doctype_to_document(name, public, system);
    }
    fn get_template_contents(&self, target: &usize) -> usize {
        self.inner.get_template_contents(target)
    }
    fn same_node(&self, left: &usize, right: &usize) -> bool {
        self.inner.same_node(left, right)
    }
    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.inner.set_quirks_mode(mode);
    }
    fn append_before_sibling(&self, sibling: &usize, node: NodeOrText<usize>) {
        self.inner.append_before_sibling(sibling, node);
    }
    fn add_attrs_if_missing(&self, target: &usize, attrs: Vec<Attribute>) {
        self.inner.add_attrs_if_missing(target, attrs);
    }
    fn remove_from_parent(&self, target: &usize) {
        self.inner.remove_from_parent(target);
    }
    fn reparent_children(&self, old: &usize, new: &usize) {
        self.inner.reparent_children(old, new);
    }
}
