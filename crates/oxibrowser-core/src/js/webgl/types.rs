//! Frontend types and GL constant mirrors for the WebGL binding layer.
//!
//! WebGL semantics that ANGLE already enforces (GL errors for bad enums,
//! shader rules under `EGL_CONTEXT_WEBGL_COMPATIBILITY_ANGLE`) are delegated
//! to the backend; this layer owns the parts that live in JS-object space:
//! resource identity, ownership, deletion state, and the small state mirrors
//! needed for spec-correct answers without GL round trips.

#![allow(dead_code)]

/// Which WebGL API a context implements. `getContext` type pinning: once a
/// canvas has a context of one kind, requesting the other returns `null`
/// (WebGL spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CtxKind {
    WebGl1,
    WebGl2,
}

impl CtxKind {
    pub fn from_type_str(s: &str) -> Option<Self> {
        match s {
            "webgl" | "experimental-webgl" => Some(Self::WebGl1),
            "webgl2" => Some(Self::WebGl2),
            _ => None,
        }
    }

    pub fn type_str(self) -> &'static str {
        match self {
            Self::WebGl1 => "webgl",
            Self::WebGl2 => "webgl2",
        }
    }

    /// EGL client version backing this context (2 = GLES2, 3 = GLES3).
    pub fn client_version(self) -> u32 {
        match self {
            Self::WebGl1 => 2,
            Self::WebGl2 => 3,
        }
    }
}

/// The three GL object kinds a WebGL program surface touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Buffer,
    Shader,
    Program,
    Texture,
    Framebuffer,
    Renderbuffer,
    VertexArray,
    Sampler,
    Query,
    UniformLocation,
}

impl ResourceKind {
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Buffer => "WebGLBuffer",
            Self::Shader => "WebGLShader",
            Self::Program => "WebGLProgram",
            Self::Texture => "WebGLTexture",
            Self::Framebuffer => "WebGLFramebuffer",
            Self::Renderbuffer => "WebGLRenderbuffer",
            Self::VertexArray => "WebGLVertexArrayObject",
            Self::Sampler => "WebGLSampler",
            Self::Query => "WebGLQuery",
            Self::UniformLocation => "WebGLUniformLocation",
        }
    }
}

/// A native GL object owned by one frontend context. The handle is the raw
/// GL name (glow native types are plain integers); it is only valid while
/// `deleted` is false and the owning context lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resource {
    pub kind: ResourceKind,
    pub handle: u32,
    pub deleted: bool,
}

/// WebGL error codes mirrored for the frontend error queue.
pub const NO_ERROR: u32 = 0;
pub const INVALID_ENUM: u32 = 0x0500;
pub const INVALID_VALUE: u32 = 0x0501;
pub const INVALID_OPERATION: u32 = 0x0502;
pub const INVALID_FRAMEBUFFER_OPERATION: u32 = 0x0506;
pub const OUT_OF_MEMORY: u32 = 0x0505;
pub const CONTEXT_LOST_WEBGL: u32 = 0x9242;

/// Max canvas drawing-buffer dimension we accept (matches GlContext guard).
pub const MAX_DIM: u32 = 16_384;

/// Pure frontend validation shared by every object-taking method.
/// Returns the GL error to enqueue, or `None` when the object is usable.
pub fn validate_resource_use(
    res: &Resource,
    expected: ResourceKind,
    owning_ctx: u64,
    calling_ctx: u64,
) -> Option<u32> {
    if owning_ctx != calling_ctx {
        // Cross-context object use: INVALID_OPERATION (WebGL spec).
        return Some(INVALID_OPERATION);
    }
    if res.kind != expected {
        return Some(INVALID_OPERATION);
    }
    if res.deleted {
        // Using a marked-for-deletion object: INVALID_OPERATION per spec.
        return Some(INVALID_OPERATION);
    }
    None
}

/// Drawing-buffer geometry derived from canvas size + context attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawingBuffer {
    pub width: u32,
    pub height: u32,
}

/// The frontend-visible context attributes actually applied at creation.
/// Mirrored 1:1 from `oxibrowser_render::gl::ContextAttrs` (kept here so the
/// JS-facing type does not depend on the render crate's feature gating).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsContextAttrs {
    pub alpha: bool,
    pub depth: bool,
    pub stencil: bool,
    pub antialias: bool,
    pub premultiplied_alpha: bool,
    pub preserve_drawing_buffer: bool,
    pub power_preference: Option<String>,
    pub fail_if_major_performance_caveat: bool,
    pub desynchronized: bool,
}

impl Default for JsContextAttrs {
    fn default() -> Self {
        Self {
            alpha: true,
            depth: true,
            stencil: false,
            antialias: false,
            premultiplied_alpha: true,
            preserve_drawing_buffer: false,
            power_preference: None,
            fail_if_major_performance_caveat: false,
            desynchronized: false,
        }
    }
}

impl JsContextAttrs {
    /// Field accessor over a JS dictionary object, decoupled from boa so the
    /// parsing rules are unit-testable without a live `Context`.
    pub fn from_js(value: &JsAttrsSource<'_>) -> Result<Self, String> {
        Ok(Self {
            alpha: value.bool("alpha").unwrap_or(true),
            depth: value.bool("depth").unwrap_or(true),
            stencil: value.bool("stencil").unwrap_or(false),
            antialias: value.bool("antialias").unwrap_or(false),
            premultiplied_alpha: value.bool("premultipliedAlpha").unwrap_or(true),
            preserve_drawing_buffer: value.bool("preserveDrawingBuffer").unwrap_or(false),
            power_preference: value.str("powerPreference").map(str::to_owned),
            fail_if_major_performance_caveat: value
                .bool("failIfMajorPerformanceCaveat")
                .unwrap_or(false),
            desynchronized: value.bool("desynchronized").unwrap_or(false),
        })
    }
}

/// Converts JS-facing attributes to the render backend's EGL attributes.
pub fn to_render_attrs(a: &JsContextAttrs) -> oxibrowser_render::gl::ContextAttrs {
    oxibrowser_render::gl::ContextAttrs {
        alpha: a.alpha,
        depth: a.depth,
        stencil: a.stencil,
        antialias: a.antialias,
        premultiplied_alpha: a.premultiplied_alpha,
        preserve_drawing_buffer: a.preserve_drawing_buffer,
        power_preference: a.power_preference.clone(),
        fail_if_major_performance_caveat: a.fail_if_major_performance_caveat,
        desynchronized: a.desynchronized,
    }
}

/// Field accessor over a JS dictionary object, decoupled from boa so the
/// parsing rules are unit-testable without a live `Context`.
pub struct JsAttrsSource<'a> {
    /// (name, is_bool, value) triples pre-extracted by the caller.
    fields: &'a [(String, bool, Option<f64>, Option<String>)],
}

impl<'a> JsAttrsSource<'a> {
    pub fn new(fields: &'a [(String, bool, Option<f64>, Option<String>)]) -> Self {
        Self { fields }
    }

    fn find(&self, name: &str) -> Option<&(String, bool, Option<f64>, Option<String>)> {
        self.fields.iter().find(|(n, _, _, _)| n == name)
    }

    fn bool(&self, name: &str) -> Option<bool> {
        self.find(name).and_then(|(_, is_bool, num, _)| {
            if *is_bool {
                num.map(|v| v != 0.0)
            } else {
                None
            }
        })
    }

    fn str(&self, name: &str) -> Option<&str> {
        self.find(name).and_then(|(_, _, _, s)| s.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Vec<(String, bool, Option<f64>, Option<String>)> {
        vec![
            ("alpha".into(), true, Some(0.0), None),
            ("depth".into(), true, None, None), // wrong type for bool → ignored
            ("premultipliedAlpha".into(), true, Some(1.0), None),
            (
                "powerPreference".into(),
                false,
                None,
                Some("high-performance".into()),
            ),
        ]
    }

    #[test]
    fn attrs_parse_bools_and_strings() {
        let f = fields();
        let src = JsAttrsSource::new(&f);
        let a = JsContextAttrs::from_js(&src).unwrap();
        assert!(!a.alpha);
        assert!(a.depth); // default, wrong-typed member ignored
        assert!(a.premultiplied_alpha);
        assert_eq!(a.power_preference.as_deref(), Some("high-performance"));
        assert!(!a.stencil);
    }

    #[test]
    fn ctx_kind_pinning_strings() {
        assert_eq!(CtxKind::from_type_str("webgl"), Some(CtxKind::WebGl1));
        assert_eq!(
            CtxKind::from_type_str("experimental-webgl"),
            Some(CtxKind::WebGl1)
        );
        assert_eq!(CtxKind::from_type_str("webgl2"), Some(CtxKind::WebGl2));
        assert_eq!(CtxKind::from_type_str("2d"), None);
    }

    #[test]
    fn cross_context_and_deleted_validation() {
        let res = Resource {
            kind: ResourceKind::Buffer,
            handle: 7,
            deleted: false,
        };
        assert_eq!(
            validate_resource_use(&res, ResourceKind::Buffer, 1, 1),
            None
        );
        assert_eq!(
            validate_resource_use(&res, ResourceKind::Buffer, 2, 1),
            Some(INVALID_OPERATION)
        );
        assert_eq!(
            validate_resource_use(&res, ResourceKind::Texture, 1, 1),
            Some(INVALID_OPERATION)
        );
        let deleted = Resource {
            deleted: true,
            ..res
        };
        assert_eq!(
            validate_resource_use(&deleted, ResourceKind::Buffer, 1, 1),
            Some(INVALID_OPERATION)
        );
    }
}
