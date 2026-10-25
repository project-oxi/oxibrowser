//! ANGLE-backed offscreen GL contexts for canvas WebGL.
//!
//! Design (per `docs/research/2026-10-25-webgl-open-source.md`):
//!
//! * One EGL display per process, created lazily on first use against the
//!   ANGLE Metal backend (`EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE`).
//! * Each canvas context gets its own `EGLContext` + pbuffer surface sized to
//!   the drawing buffer. WebGL semantics (validation, shader rules, GL errors)
//!   come from ANGLE's WebGL compatibility mode
//!   (`EGL_CONTEXT_WEBGL_COMPATIBILITY_ANGLE`) plus robust resource
//!   initialization; bind-generates-resource is disabled as WebGL requires.
//! * Contexts are created, used, and destroyed on the JS thread — the same
//!   thread that owns the `RenderDocument` — so no command marshaling is
//!   needed. Only one context is current per thread at a time; switching is
//!   tracked to avoid redundant `eglMakeCurrent` calls.
//! * Page compositing uses readback: `read_pixels_rgba` returns the drawing
//!   buffer top-down, and clears it afterwards unless
//!   `preserveDrawingBuffer` is set (WebGL presentation semantics).

pub mod egl;

use std::ffi::{CStr, CString};
use std::sync::{LazyLock, OnceLock};

use glow::HasContext;

/// Failures surfaced by the GL backend. Every variant is meant to reach the
/// web-visible layer (console error / null `getContext`) instead of being
/// silently swallowed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum GlError {
    #[error("EGL display init failed: {0} ({1})")]
    DisplayInit(String, &'static str),
    #[error("EGL context creation failed ({0})")]
    ContextInit(&'static str),
    #[error("EGL surface creation failed for {width}x{height} ({code})")]
    SurfaceInit { width: u32, height: u32, code: i32 },
    #[error("eglMakeCurrent failed ({0})")]
    MakeCurrent(&'static str),
    #[error("WebGL unsupported: {0}")]
    Unsupported(String),
    #[error("context lost")]
    Lost,
    #[error("invalid canvas size {width}x{height}")]
    BadSize { width: u32, height: u32 },
}

/// Static information about the display, exposed for CDP diagnostics and the
/// `WEBGL_debug_renderer_info`-style strings.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub egl_vendor: String,
    pub egl_version: String,
    pub egl_extensions: Vec<String>,
    /// `GL_VENDOR` / `GL_RENDERER` / `GL_VERSION` — filled when the first
    /// context is made current.
    pub gl_vendor: OnceLock<String>,
    pub gl_renderer: OnceLock<String>,
    pub gl_version: String,
}

impl DisplayInfo {
    fn from_display(dpy: egl::EGLDisplay) -> Result<Self, GlError> {
        let vendor = query_string(dpy, egl::EGL_VENDOR)?;
        let version = query_string(dpy, egl::EGL_VERSION)?;
        let extensions = query_string(dpy, egl::EGL_EXTENSIONS)?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        Ok(Self {
            egl_vendor: vendor,
            egl_version: version,
            egl_extensions: extensions,
            gl_vendor: OnceLock::new(),
            gl_renderer: OnceLock::new(),
            gl_version: String::new(),
        })
    }

    /// True when the display supports the WebGL compatibility context flag.
    pub fn webgl_compatible(&self) -> bool {
        self.egl_extensions
            .iter()
            .any(|e| e == "EGL_ANGLE_create_context_webgl_compatibility")
    }
}

struct GlDisplayInner {
    dpy: egl::EGLDisplay,
    info: DisplayInfo,
    /// Created lazily on the first context creation — the loader validates
    /// `GL_VERSION` at construction, which requires a current context.
    loader: OnceLock<glow::Context>,
}

// Safety: the EGL display is an immutable handle after initialization and EGL
// display calls are thread-safe by spec. We additionally confine all context
// usage to the JS thread by construction; the static only shares the display
// handle, the cached `DisplayInfo`, and the loader (function pointers).
unsafe impl Send for GlDisplayInner {}
unsafe impl Sync for GlDisplayInner {}

impl GlDisplayInner {
    pub fn info(&self) -> &DisplayInfo {
        &self.info
    }

    /// GL loader. Valid only after at least one [`GlContext`] was created
    /// (construction reads `GL_VERSION` on the current context).
    pub fn loader(&self) -> Option<&glow::Context> {
        self.loader.get()
    }

    fn ensure_loader(&self) -> &glow::Context {
        self.loader.get_or_init(|| unsafe {
            glow::Context::from_loader_function(|name: &str| {
                let c = CString::new(name).expect("no NUL in GL proc name");
                egl::eglGetProcAddress(c.as_ptr())
            })
        })
    }
}

static DISPLAY: LazyLock<Result<GlDisplayInner, GlError>> = LazyLock::new(init_display);

/// Process-wide ANGLE display + GL loader. Safe to call repeatedly; only the
/// first call initializes. Must be called from the thread that will use the
/// contexts (the JS thread) — EGL entry points thread-switch cheaply, but we
/// keep all context usage on one thread by construction.
fn display() -> Result<&'static GlDisplayInner, GlError> {
    match &*DISPLAY {
        Ok(inner) => Ok(inner),
        Err(e) => Err(e.clone()),
    }
}

/// Display metadata (`EGL_VENDOR` / `EGL_VERSION` / extension list).
pub fn display_info() -> Result<&'static DisplayInfo, GlError> {
    Ok(display()?.info())
}

/// The GL loader to use while a [`GlContext`] is current. Fails before any
/// [`GlContext`] has been created (the loader needs a current context).
pub fn loader() -> Result<&'static glow::Context, GlError> {
    display()?
        .loader()
        .ok_or_else(|| GlError::Unsupported("no GL context created yet".into()))
}

/// Backend selection for experiments: OXI_ANGLE_PLATFORM=vulkan|metal
/// (default metal). See docs/designs/2026-10-25-webgl-angle.md — ANGLE Metal
/// does not rasterize draws on some macOS 26 hosts; ANGLE→Vulkan→MoltenVK is
/// the verified fallback there.
pub fn platform_type() -> egl::EGLint {
    match std::env::var("OXI_ANGLE_PLATFORM").as_deref() {
        Ok("vulkan") => egl::EGL_PLATFORM_ANGLE_TYPE_VULKAN_ANGLE,
        _ => egl::EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE,
    }
}

fn init_display() -> Result<GlDisplayInner, GlError> {
    unsafe {
        let attribs = [
            egl::EGL_PLATFORM_ANGLE_TYPE_ANGLE,
            platform_type(),
            egl::EGL_PLATFORM_ANGLE_MAX_VERSION_MAJOR_ANGLE,
            egl::EGL_DONT_CARE,
            egl::EGL_PLATFORM_ANGLE_MAX_VERSION_MINOR_ANGLE,
            egl::EGL_DONT_CARE,
            egl::EGL_NONE,
        ];
        let dpy = egl::eglGetPlatformDisplayEXT(
            egl::EGL_PLATFORM_ANGLE_ANGLE as egl::EGLenum,
            egl::EGL_DEFAULT_DISPLAY,
            attribs.as_ptr(),
        );
        if dpy.is_null() {
            return Err(GlError::DisplayInit(
                "eglGetPlatformDisplayEXT returned EGL_NO_DISPLAY".into(),
                egl::error_name(egl::last_error()),
            ));
        }
        let (mut major, mut minor) = (0 as egl::EGLint, 0 as egl::EGLint);
        if egl::eglInitialize(dpy, &mut major, &mut minor) == egl::EGL_FALSE {
            return Err(GlError::DisplayInit(
                format!("eglInitialize failed (v{major}.{minor} requested)"),
                egl::error_name(egl::last_error()),
            ));
        }
        let info = DisplayInfo::from_display(dpy)?;
        // EGL thread API defaults to "none" in ANGLE; without this every
        // context creation fails with EGL_BAD_MATCH ("Unsupported API.").
        if egl::eglBindAPI(egl::EGL_OPENGL_ES_API) == egl::EGL_FALSE {
            return Err(GlError::DisplayInit(
                "eglBindAPI(EGL_OPENGL_ES_API) failed".to_string(),
                egl::error_name(egl::last_error()),
            ));
        }
        Ok(GlDisplayInner {
            dpy,
            info,
            loader: OnceLock::new(),
        })
    }
}

fn query_string(dpy: egl::EGLDisplay, which: egl::EGLint) -> Result<String, GlError> {
    unsafe {
        let ptr = egl::eglQueryString(dpy, which);
        if ptr.is_null() {
            return Err(GlError::DisplayInit(
                "eglQueryString returned NULL".into(),
                egl::error_name(egl::last_error()),
            ));
        }
        Ok(CStr::from_ptr(ptr).to_string_lossy().into_owned())
    }
}

/// Attributes of a WebGL context (`canvas.getContext(type, attrs)`), stored
/// so `getContextAttributes` returns what was actually applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextAttrs {
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

impl Default for ContextAttrs {
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

/// An offscreen drawing surface bound to one `EGLContext`.
pub struct GlContext {
    dpy: egl::EGLDisplay,
    ctx: egl::EGLContext,
    surface: egl::EGLSurface,
    pub width: u32,
    pub height: u32,
    /// 2 = WebGL1 (GLES2), 3 = WebGL2 (GLES3).
    pub client_version: u32,
    pub attrs: ContextAttrs,
    pub lost: bool,
}

impl GlContext {
    /// Creates a WebGL-compatible context and a pbuffer drawing surface.
    /// Must be called on the JS thread.
    pub fn new(
        client_version: u32,
        width: u32,
        height: u32,
        attrs: ContextAttrs,
    ) -> Result<Self, GlError> {
        if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
            return Err(GlError::BadSize { width, height });
        }
        let display = display()?;
        if !display.info.webgl_compatible() {
            return Err(GlError::Unsupported(
                "EGL display lacks EGL_ANGLE_create_context_webgl_compatibility".into(),
            ));
        }
        unsafe {
            // Per-thread EGL client API — must be bound on the thread that
            // creates the context (ANGLE defaults to none → EGL_BAD_MATCH).
            egl::eglBindAPI(egl::EGL_OPENGL_ES_API);
            let config = choose_config(display.dpy, client_version, &attrs)?;

            let mut ctx_attribs = vec![
                egl::EGL_CONTEXT_CLIENT_VERSION,
                client_version as egl::EGLint,
                egl::EGL_CONTEXT_WEBGL_COMPATIBILITY_ANGLE,
                egl::EGL_TRUE as egl::EGLint,
                egl::EGL_CONTEXT_BIND_GENERATES_RESOURCE_CHROMIUM,
                egl::EGL_FALSE as egl::EGLint,
                egl::EGL_ROBUST_RESOURCE_INITIALIZATION_ANGLE,
                egl::EGL_TRUE as egl::EGLint,
                egl::EGL_NONE,
            ];
            let ctx = egl::eglCreateContext(
                display.dpy,
                config,
                egl::EGL_NO_CONTEXT,
                ctx_attribs.as_mut_ptr(),
            );
            if ctx.is_null() {
                return Err(GlError::ContextInit(egl::error_name(egl::last_error())));
            }

            let surface =
                Self::create_surface(display.dpy, config, width, height).map_err(|code| {
                    egl::eglDestroyContext(display.dpy, ctx);
                    GlError::SurfaceInit {
                        width,
                        height,
                        code,
                    }
                })?;

            if egl::eglMakeCurrent(display.dpy, surface, surface, ctx) == egl::EGL_FALSE {
                let code = egl::last_error();
                egl::eglDestroySurface(display.dpy, surface);
                egl::eglDestroyContext(display.dpy, ctx);
                return Err(GlError::MakeCurrent(egl::error_name(code)));
            }

            let gl = display.ensure_loader();
            let _ = display
                .info
                .gl_vendor
                .set(gl.get_parameter_string(glow::VENDOR));
            let _ = display
                .info
                .gl_renderer
                .set(gl.get_parameter_string(glow::RENDERER));

            Ok(Self {
                dpy: display.dpy,
                ctx,
                surface,
                width,
                height,
                client_version,
                attrs,
                lost: false,
            })
        }
    }

    unsafe fn create_surface(
        dpy: egl::EGLDisplay,
        config: egl::EGLConfig,
        width: u32,
        height: u32,
    ) -> Result<egl::EGLSurface, i32> {
        let attribs = [
            egl::EGL_WIDTH,
            width as egl::EGLint,
            egl::EGL_HEIGHT,
            height as egl::EGLint,
            egl::EGL_NONE,
        ];
        let surface = unsafe { egl::eglCreatePbufferSurface(dpy, config, attribs.as_ptr()) };
        if surface.is_null() {
            return Err(egl::last_error());
        }
        Ok(surface)
    }

    /// Makes this context current on the calling thread. The caller tracks the
    /// already-current context to avoid redundant calls.
    pub fn make_current(&self) -> Result<(), GlError> {
        if self.lost {
            return Err(GlError::Lost);
        }
        unsafe {
            if egl::eglMakeCurrent(self.dpy, self.surface, self.surface, self.ctx) == egl::EGL_FALSE
            {
                return Err(GlError::MakeCurrent(egl::error_name(egl::last_error())));
            }
        }
        Ok(())
    }

    /// Unbinds this context from the calling thread so another EGL context
    /// (possibly another canvas) can bind cleanly afterwards.
    pub fn make_not_current(&self) -> Result<(), GlError> {
        if self.lost {
            return Ok(());
        }
        unsafe {
            if egl::eglMakeCurrent(
                self.dpy,
                egl::EGL_NO_SURFACE,
                egl::EGL_NO_SURFACE,
                egl::EGL_NO_CONTEXT,
            ) == egl::EGL_FALSE
            {
                return Err(GlError::MakeCurrent(egl::error_name(egl::last_error())));
            }
        }
        Ok(())
    }

    /// Recreates the drawing surface at a new size. GL object state survives
    /// (per WebGL resize semantics); the drawing buffer is cleared.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GlError> {
        if width == self.width && height == self.height {
            return Ok(());
        }
        if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
            return Err(GlError::BadSize { width, height });
        }
        unsafe {
            let config = choose_config(self.dpy, self.client_version, &self.attrs)?;
            let surface =
                Self::create_surface(self.dpy, config, width, height).map_err(|code| {
                    GlError::SurfaceInit {
                        width,
                        height,
                        code,
                    }
                })?;
            if egl::eglMakeCurrent(self.dpy, surface, surface, self.ctx) == egl::EGL_FALSE {
                let code = egl::last_error();
                egl::eglDestroySurface(self.dpy, surface);
                return Err(GlError::MakeCurrent(egl::error_name(code)));
            }
            // The new surface is current now, so the old one can be destroyed.
            if self.surface != egl::EGL_NO_SURFACE {
                egl::eglDestroySurface(self.dpy, self.surface);
            }
            self.surface = surface;
            self.width = width;
            self.height = height;
        }
        Ok(())
    }

    /// Reads the drawing buffer as top-down RGBA8. When `present` is true the
    /// buffer is cleared afterwards unless `preserveDrawingBuffer` was set
    /// (mirrors WebGL's clear-after-compositing semantics).
    pub fn read_pixels_rgba(&self, gl: &glow::Context, present: bool) -> Vec<u8> {
        let (w, h) = (self.width as i32, self.height as i32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        unsafe {
            gl.read_pixels(
                0,
                0,
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
            if present && !self.attrs.preserve_drawing_buffer {
                gl.clear(
                    glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT | glow::STENCIL_BUFFER_BIT,
                );
            }
        }
        if !self.attrs.alpha {
            for px in buf.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        }
        // GL origin is bottom-left; canvas bitmaps are top-down.
        let stride = (w * 4) as usize;
        let mut flipped = vec![0u8; buf.len()];
        for row in 0..h as usize {
            let src = &buf[row * stride..(row + 1) * stride];
            flipped[(h as usize - 1 - row) * stride..(h as usize - row) * stride]
                .copy_from_slice(src);
        }
        flipped
    }

    /// Returns the display the context belongs to.
    pub fn display(&self) -> egl::EGLDisplay {
        self.dpy
    }

    /// Tears down surface + context. Must run on the owning (JS) thread;
    /// releasing current first so a later context can bind cleanly.
    pub fn destroy(mut self) {
        unsafe {
            egl::eglMakeCurrent(
                self.dpy,
                egl::EGL_NO_SURFACE,
                egl::EGL_NO_SURFACE,
                egl::EGL_NO_CONTEXT,
            );
            if self.surface != egl::EGL_NO_SURFACE {
                egl::eglDestroySurface(self.dpy, self.surface);
                self.surface = egl::EGL_NO_SURFACE;
            }
            if self.ctx != egl::EGL_NO_CONTEXT {
                egl::eglDestroyContext(self.dpy, self.ctx);
                self.ctx = egl::EGL_NO_CONTEXT;
            }
        }
    }
}

impl Drop for GlContext {
    fn drop(&mut self) {
        // Safety net for paths that never call `destroy` explicitly; still on
        // the owning thread because the value lives in the JS-thread registry.
        unsafe {
            if self.ctx != egl::EGL_NO_CONTEXT || self.surface != egl::EGL_NO_SURFACE {
                egl::eglMakeCurrent(
                    self.dpy,
                    egl::EGL_NO_SURFACE,
                    egl::EGL_NO_SURFACE,
                    egl::EGL_NO_CONTEXT,
                );
            }
            if self.surface != egl::EGL_NO_SURFACE {
                egl::eglDestroySurface(self.dpy, self.surface);
            }
            if self.ctx != egl::EGL_NO_CONTEXT {
                egl::eglDestroyContext(self.dpy, self.ctx);
            }
        }
    }
}

fn choose_config(
    dpy: egl::EGLDisplay,
    client_version: u32,
    attrs: &ContextAttrs,
) -> Result<egl::EGLConfig, GlError> {
    let renderable = if client_version >= 3 {
        egl::EGL_OPENGL_ES3_BIT
    } else {
        egl::EGL_OPENGL_ES2_BIT
    };
    let attribs = [
        egl::EGL_SURFACE_TYPE,
        egl::EGL_PBUFFER_BIT,
        egl::EGL_RENDERABLE_TYPE,
        renderable,
        egl::EGL_RED_SIZE,
        8,
        egl::EGL_GREEN_SIZE,
        8,
        egl::EGL_BLUE_SIZE,
        8,
        egl::EGL_ALPHA_SIZE,
        8,
        egl::EGL_DEPTH_SIZE,
        if attrs.depth { 24 } else { 0 },
        egl::EGL_STENCIL_SIZE,
        if attrs.stencil { 8 } else { 0 },
        egl::EGL_SAMPLES,
        0,
        egl::EGL_SAMPLE_BUFFERS,
        0,
        egl::EGL_NONE,
    ];
    unsafe {
        let mut config = std::ptr::null_mut();
        let mut num = 0 as egl::EGLint;
        if egl::eglChooseConfig(dpy, attribs.as_ptr(), &mut config, 1, &mut num) == egl::EGL_FALSE
            || num < 1
        {
            return Err(GlError::ContextInit(egl::error_name(egl::last_error())));
        }
        Ok(config)
    }
}
