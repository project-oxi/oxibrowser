//! Minimal hand-written EGL 1.5 FFI surface used by the ANGLE backend.
//!
//! Only the entry points and constants OxiBrowser needs are declared. Values
//! were transcribed from ANGLE `include/EGL/{egl.h,eglext.h,eglext_angle.h}`
//! at revision `chromium/8118` (the revision pinned by
//! `scripts/setup-angle.sh`), not from memory.

#![allow(non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_void};

pub type EGLDisplay = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLSurface = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLBoolean = u32;
pub type EGLenum = u32;
pub type EGLint = i32;

pub const EGL_FALSE: EGLBoolean = 0;
pub const EGL_TRUE: EGLBoolean = 1;
/// NULL handle sentinel for display/surface/context/config.
pub const EGL_NO_DISPLAY: EGLDisplay = std::ptr::null_mut();
pub const EGL_NO_SURFACE: EGLSurface = std::ptr::null_mut();
pub const EGL_NO_CONTEXT: EGLContext = std::ptr::null_mut();
pub const EGL_DONT_CARE: EGLint = -1;
pub const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut();

// eglGetError results
pub const EGL_SUCCESS: EGLint = 0x3000;
pub const EGL_NOT_INITIALIZED: EGLint = 0x3001;
pub const EGL_BAD_ACCESS: EGLint = 0x3002;
pub const EGL_BAD_ALLOC: EGLint = 0x3003;
pub const EGL_BAD_ATTRIBUTE: EGLint = 0x3004;
pub const EGL_BAD_CONFIG: EGLint = 0x3005;
pub const EGL_BAD_CONTEXT: EGLint = 0x3006;
pub const EGL_BAD_CURRENT_SURFACE: EGLint = 0x3007;
pub const EGL_BAD_DISPLAY: EGLint = 0x3008;
pub const EGL_BAD_MATCH: EGLint = 0x3009;
pub const EGL_BAD_NATIVE_PIXMAP: EGLint = 0x300A;
pub const EGL_BAD_NATIVE_WINDOW: EGLint = 0x300B;
pub const EGL_BAD_PARAMETER: EGLint = 0x300C;
pub const EGL_BAD_SURFACE: EGLint = 0x300D;

// Surface / config attributes
pub const EGL_BUFFER_SIZE: EGLint = 0x3020;
pub const EGL_ALPHA_SIZE: EGLint = 0x3021;
pub const EGL_BLUE_SIZE: EGLint = 0x3022;
pub const EGL_GREEN_SIZE: EGLint = 0x3023;
pub const EGL_RED_SIZE: EGLint = 0x3024;
pub const EGL_DEPTH_SIZE: EGLint = 0x3025;
pub const EGL_STENCIL_SIZE: EGLint = 0x3026;
pub const EGL_SURFACE_TYPE: EGLint = 0x3033;
pub const EGL_PBUFFER_BIT: EGLint = 0x0001;
pub const EGL_SAMPLES: EGLint = 0x3031;
pub const EGL_SAMPLE_BUFFERS: EGLint = 0x3032;
pub const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
pub const EGL_WIDTH: EGLint = 0x3057;
pub const EGL_HEIGHT: EGLint = 0x3056;
pub const EGL_NONE: EGLint = 0x3038;
pub const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
pub const EGL_OPENGL_ES3_BIT: EGLint = 0x0040;
/// EGL 1.5 `eglGetPlatformDisplay` platform token for EGL itself.
pub const EGL_PLATFORM_EGL: EGLenum = 0x31D8;

// EGL_ANGLE_platform_angle (eglext_angle.h @ chromium/8118)
pub const EGL_PLATFORM_ANGLE_ANGLE: EGLint = 0x3202;
pub const EGL_PLATFORM_ANGLE_TYPE_ANGLE: EGLint = 0x3203;
pub const EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE: EGLint = 0x3489;
pub const EGL_PLATFORM_ANGLE_TYPE_OPENGL_ANGLE: EGLint = 0x320D;
pub const EGL_PLATFORM_ANGLE_TYPE_VULKAN_ANGLE: EGLint = 0x3450;
pub const EGL_PLATFORM_ANGLE_MAX_VERSION_MAJOR_ANGLE: EGLint = 0x3204;
pub const EGL_PLATFORM_ANGLE_MAX_VERSION_MINOR_ANGLE: EGLint = 0x3205;
pub const EGL_PLATFORM_ANGLE_DEBUG_LAYERS_ENABLED_ANGLE: EGLint = 0x3451;

// EGL_ANGLE_create_context_webgl_compatibility / robust resource init
pub const EGL_CONTEXT_WEBGL_COMPATIBILITY_ANGLE: EGLint = 0x33AC;
pub const EGL_CONTEXT_BIND_GENERATES_RESOURCE_CHROMIUM: EGLint = 0x33AD;
pub const EGL_ROBUST_RESOURCE_INITIALIZATION_ANGLE: EGLint = 0x3453;

// EGL 1.5 context attributes
/// Legacy `EGL_CONTEXT_CLIENT_VERSION` — the spelling this ANGLE build's
/// `eglCreateContext` accepts (the `EGL_CONTEXT_MAJOR_VERSION` alias 0x30FB
/// returns EGL_BAD_MATCH on chromium/8118 static builds).
pub const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
pub const EGL_CONTEXT_MINOR_VERSION: EGLint = 0x30FC;

// Client API tokens (eglBindAPI / eglQueryAPI)
pub const EGL_OPENGL_ES_API: EGLenum = 0x30A0;
pub const EGL_OPENGL_API: EGLenum = 0x30A2;

// eglQueryString tokens
pub const EGL_EXTENSIONS: EGLint = 0x3055;
pub const EGL_VERSION: EGLint = 0x3054;
pub const EGL_VENDOR: EGLint = 0x3053;

// Surface attributes (query)
pub const EGL_SURFACE_ORIENTATION_ANGLE: EGLint = 0x33A8;
pub const EGL_SURFACE_ORIENTATION_INVERT_Y_ANGLE: EGLint = 0x0002;

unsafe extern "C" {
    pub fn eglGetError() -> EGLint;
    pub fn eglGetCurrentContext() -> EGLContext;
    pub fn eglGetDisplay(native_display: *mut c_void) -> EGLDisplay;
    pub fn eglBindAPI(api: EGLenum) -> EGLBoolean;
    pub fn eglQueryAPI() -> EGLenum;
    pub fn eglGetConfigs(
        dpy: EGLDisplay,
        configs: *mut EGLConfig,
        config_size: EGLint,
        num_config: *mut EGLint,
    ) -> EGLBoolean;
    pub fn eglGetConfigAttrib(
        dpy: EGLDisplay,
        config: EGLConfig,
        attribute: EGLint,
        value: *mut EGLint,
    ) -> EGLBoolean;
    pub fn eglGetPlatformDisplayEXT(
        platform: EGLenum,
        native_display: *mut c_void,
        attrib_list: *const EGLint,
    ) -> EGLDisplay;
    pub fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
    pub fn eglTerminate(dpy: EGLDisplay) -> EGLBoolean;
    pub fn eglQueryString(dpy: EGLDisplay, name: EGLint) -> *const c_char;
    pub fn eglChooseConfig(
        dpy: EGLDisplay,
        attrib_list: *const EGLint,
        configs: *mut EGLConfig,
        config_size: EGLint,
        num_config: *mut EGLint,
    ) -> EGLBoolean;
    pub fn eglCreateContext(
        dpy: EGLDisplay,
        config: EGLConfig,
        share_context: EGLContext,
        attrib_list: *const EGLint,
    ) -> EGLContext;
    pub fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
    pub fn eglCreatePbufferSurface(
        dpy: EGLDisplay,
        config: EGLConfig,
        attrib_list: *const EGLint,
    ) -> EGLSurface;
    pub fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    pub fn eglMakeCurrent(
        dpy: EGLDisplay,
        draw: EGLSurface,
        read: EGLSurface,
        ctx: EGLContext,
    ) -> EGLBoolean;
    pub fn eglQuerySurface(
        dpy: EGLDisplay,
        surface: EGLSurface,
        attribute: EGLint,
        value: *mut EGLint,
    ) -> EGLBoolean;
    pub fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

/// Returns the EGL error currently flagged, consumed via `eglGetError`.
pub fn last_error() -> EGLint {
    unsafe { eglGetError() }
}

/// Describes an EGL error for logs / JS-visible failure reporting.
pub fn error_name(code: EGLint) -> &'static str {
    match code {
        EGL_SUCCESS => "EGL_SUCCESS",
        EGL_NOT_INITIALIZED => "EGL_NOT_INITIALIZED",
        EGL_BAD_ACCESS => "EGL_BAD_ACCESS",
        EGL_BAD_ALLOC => "EGL_BAD_ALLOC",
        EGL_BAD_ATTRIBUTE => "EGL_BAD_ATTRIBUTE",
        EGL_BAD_CONFIG => "EGL_BAD_CONFIG",
        EGL_BAD_CONTEXT => "EGL_BAD_CONTEXT",
        EGL_BAD_CURRENT_SURFACE => "EGL_BAD_CURRENT_SURFACE",
        EGL_BAD_DISPLAY => "EGL_BAD_DISPLAY",
        EGL_BAD_MATCH => "EGL_BAD_MATCH",
        EGL_BAD_NATIVE_PIXMAP => "EGL_BAD_NATIVE_PIXMAP",
        EGL_BAD_NATIVE_WINDOW => "EGL_BAD_NATIVE_WINDOW",
        EGL_BAD_PARAMETER => "EGL_BAD_PARAMETER",
        EGL_BAD_SURFACE => "EGL_BAD_SURFACE",
        _ => "EGL_UNKNOWN",
    }
}
