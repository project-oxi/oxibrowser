//! Chrome-like browser fingerprint surface (Level-1 stealth).
//!
//! oxibrowser is a pure-Rust engine (boa_engine + html5ever): the browser
//! APIs that real-Chrome stealth scripts *patch* (`navigator.webdriver`,
//! `navigator.plugins`, `window.chrome`, `WebGLRenderingContext`, …) do not
//! exist here at all. This module therefore *creates* them with the values a
//! non-headless desktop Chrome would report, instead of patching real ones
//! the way omp's puppeteer stealth does (`evaluateOnNewDocument` → V8).
//!
//! ## Scope — JS surface ONLY
//!
//! Defeats naive/medium bot detection that reads `navigator.webdriver`,
//! `navigator.plugins.length`, `window.chrome`, `navigator.permissions.query`,
//! and `navigator.userAgentData`. It does **not** touch the TLS/HTTP-2
//! transport fingerprint — that is `wreq`'s job (`Emulation::Chrome149`
//! transport emulation in `network/client.rs`, not this module) — nor
//! boa↔V8 behavioural fidelity, so **correlating** detection
//! (Cloudflare/DataDome/CreepJS) will still flag the cross-layer mismatch.
//! That gap is closed by the **pure-Rust cross-layer stealth design** — a
//! `ChromeProfile` driving the `wreq` transport (Chrome TLS/HTTP-2
//! emulation) and this JS surface to a mutually-consistent Chrome
//! fingerprint — NOT by adopting real Chromium.
//! See `docs/designs/2026-06-25-pure-rust-stealth.md`.
//!
//! ## Profile coherence
//!
//! Every OS-derived value flows from one [`ChromeProfile`], parsed from the
//! user agent oxibrowser sends over the wire. `navigator.platform`,
//! `userAgentData.platform`, the WebGL renderer, and the Sec-CH-UA brands all
//! agree on a single OS + Chrome major version — an impossible-on-real-hardware
//! combination (e.g. `MacIntel` + `Direct3D11`) is exactly what correlating
//! detectors score highest, so the profile is kept atomic on purpose.

use boa_engine::{
    Context, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string,
    object::{FunctionObjectBuilder, ObjectInitializer},
    property::Attribute,
};

// ── Chrome profile ───────────────────────────────────────────────────────

/// A single coherent real-Chrome profile. All OS-derived fingerprint values
/// are derived from this so they cannot drift out of sync.
///
/// Construct via [`ChromeProfile::from_ua`], which infers the OS family from
/// the UA string oxibrowser actually sends.
pub(crate) struct ChromeProfile {
    /// `navigator.platform` ("Win32" | "MacIntel" | "Linux x86_64").
    platform: &'static str,
    /// Sec-CH-UA platform ("Windows" | "macOS" | "Linux").
    ua_data_platform: &'static str,
    /// Chrome major version (single source of truth for brands + appVersion).
    major: u32,
    /// Chrome full version ("131.0.6778.139").
    full_version: &'static str,
    /// `UNMASKED_VENDOR_WEBGL` (37445).
    webgl_vendor: &'static str,
    /// `UNMASKED_RENDERER_WEBGL` (37446) — OS-appropriate ANGLE backend.
    webgl_renderer: &'static str,
    /// Architecture hint for userAgentData.getHighEntropyValues.
    architecture: &'static str,
    /// Bitness hint for userAgentData.getHighEntropyValues.
    bitness: &'static str,
    /// Platform version hint (Windows NT / macOS / Linux kernel).
    platform_version: &'static str,
}

// ANGLE renderer strings for each OS family — these are the strings a real
// Chrome on the matching OS reports via WEBGL_debug_renderer_info. Direct3D11
// is Windows-only; macOS/Linux use OpenGL/Metal-backed ANGLE. Mixing these
// across OSes is a textbook cross-property inconsistency.
const WEBGL_RENDERER_WINDOWS: &str =
    "ANGLE (Intel, Intel(R) UHD Graphics 630 Direct3D11 vs_5_0 ps_5_0)";
const WEBGL_RENDERER_MACOS: &str =
    "ANGLE (Intel Inc., Intel(R) Iris(TM) Plus Graphics 645, OpenGL 4.1)";
const WEBGL_RENDERER_LINUX: &str = "ANGLE (Intel, Intel(R) UHD Graphics 630 (CML GT2), OpenGL 4.6)";

impl ChromeProfile {
    /// Default Chrome major if the UA does not advertise one.
    const DEFAULT_MAJOR: u32 = 131;
    const DEFAULT_FULL: &'static str = "131.0.6778.139";

    /// Infer a coherent profile from a user agent string.
    ///
    /// The OS family is read from the UA so the JS surface agrees with the
    /// UA oxibrowser sends over the wire (no MacIntel-with-Direct3D11 trap).
    pub(crate) fn from_ua(ua: &str) -> Self {
        let (platform, ua_data_platform, webgl_renderer, architecture, platform_version) =
            if ua.contains("Windows") {
                ("Win32", "Windows", WEBGL_RENDERER_WINDOWS, "x86", "15.0.0")
            } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
                ("MacIntel", "macOS", WEBGL_RENDERER_MACOS, "arm", "14.0.0")
            } else {
                (
                    "Linux x86_64",
                    "Linux",
                    WEBGL_RENDERER_LINUX,
                    "x86",
                    "6.5.0",
                )
            };

        let major = chrome_major(ua).unwrap_or(Self::DEFAULT_MAJOR);

        Self {
            platform,
            ua_data_platform,
            major,
            full_version: Self::DEFAULT_FULL,
            webgl_vendor: "Google Inc. (Intel)",
            webgl_renderer,
            architecture,
            bitness: "64",
            platform_version,
        }
    }

    /// The `navigator.platform` for a UA — single source of truth so the
    /// platform can never disagree with the WebGL renderer or userAgentData.
    pub(crate) fn platform_for(user_agent: &str) -> &'static str {
        Self::from_ua(user_agent).platform
    }
}

/// Extract the Chrome major version (`Chrome/<n>`) from a UA string.
fn chrome_major(ua: &str) -> Option<u32> {
    let idx = ua.find("Chrome/")? + "Chrome/".len();
    ua[idx..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|s| s.parse().ok())
}

// ── Stealth surface ──────────────────────────────────────────────────────

/// All Level-1 stealth objects, pre-built and ready to attach. Built by
/// [`build`] from a [`ChromeProfile`].
pub struct StealthSurface {
    pub chrome: JsValue,
    /// `navigator.plugins` (PluginArray-like).
    pub plugins: JsValue,
    /// `navigator.mimeTypes` (MimeTypeArray-like).
    pub mime_types: JsValue,
    /// `navigator.userAgentData`.
    pub user_agent_data: JsValue,
    /// `navigator.permissions`.
    pub permissions: JsValue,
    /// `navigator.connection`.
    pub connection: JsValue,
    /// `window.WebGLRenderingContext` constructor.
    pub webgl1: JsValue,
    /// `window.WebGL2RenderingContext` constructor.
    pub webgl2: JsValue,
}

/// Build the full Level-1 surface. `user_agent` must be the UA oxibrowser
/// sends over the wire (`config.user_agent`) so the profile aligns with it.
pub fn build(ctx: &mut Context, user_agent: &str) -> StealthSurface {
    let profile = ChromeProfile::from_ua(user_agent);
    let (plugins, mime_types) = build_plugin_and_mime_arrays(ctx);
    StealthSurface {
        chrome: build_chrome_object(ctx),
        plugins: plugins.into(),
        mime_types: mime_types.into(),
        user_agent_data: build_user_agent_data(ctx, &profile),
        permissions: build_permissions(ctx),
        connection: build_connection(ctx),
        webgl1: build_webgl_constructor(ctx, &profile, false),
        webgl2: build_webgl_constructor(ctx, &profile, true),
    }
}

/// Attach the navigator-level stealth properties (`plugins`, `mimeTypes`,
/// `userAgentData`, `permissions`, `connection`) to an already-built
/// `navigator` object. Because `navigator` is shallow-shared between
/// `window.navigator` and the global `navigator`, this propagates to both.
pub fn attach_to_navigator(
    ctx: &mut Context,
    nav: &JsObject,
    surface: &StealthSurface,
) -> JsResult<()> {
    nav.set(js_string!("plugins"), surface.plugins.clone(), true, ctx)?;
    nav.set(
        js_string!("mimeTypes"),
        surface.mime_types.clone(),
        true,
        ctx,
    )?;
    nav.set(
        js_string!("userAgentData"),
        surface.user_agent_data.clone(),
        true,
        ctx,
    )?;
    nav.set(
        js_string!("permissions"),
        surface.permissions.clone(),
        true,
        ctx,
    )?;
    nav.set(
        js_string!("connection"),
        surface.connection.clone(),
        true,
        ctx,
    )?;
    Ok(())
}

// ── window.chrome ────────────────────────────────────────────────────────

/// `window.chrome` — detection checks presence + `chrome.runtime`. Minimal
/// but realistic: `app`, `runtime`, `csi`, `loadTimes`.
fn build_chrome_object(ctx: &mut Context) -> JsValue {
    let app = ObjectInitializer::new(ctx)
        .property(
            js_string!("isInstalled"),
            JsValue::from(true),
            Attribute::all(),
        )
        .build();
    let runtime = ObjectInitializer::new(ctx).build();
    let csi_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let o = ObjectInitializer::new(ctx)
                .property(js_string!("startE"), JsValue::from(0.0), Attribute::all())
                .property(js_string!("onloadT"), JsValue::from(0.0), Attribute::all())
                .property(js_string!("pageT"), JsValue::from(0.0), Attribute::all())
                .property(js_string!("tran"), JsValue::from(0), Attribute::all())
                .build();
            Ok(JsValue::from(o))
        })
    };
    let load_times_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let o = ObjectInitializer::new(ctx)
                .property(
                    js_string!("requestTime"),
                    JsValue::from(0.0),
                    Attribute::all(),
                )
                .property(
                    js_string!("startLoadTime"),
                    JsValue::from(0.0),
                    Attribute::all(),
                )
                .property(
                    js_string!("commitLoadTime"),
                    JsValue::from(0.0),
                    Attribute::all(),
                )
                .property(
                    js_string!("finishDocumentLoadTime"),
                    JsValue::from(0.0),
                    Attribute::all(),
                )
                .property(
                    js_string!("finishLoadTime"),
                    JsValue::from(0.0),
                    Attribute::all(),
                )
                .property(
                    js_string!("firstPaintTime"),
                    JsValue::from(0.0),
                    Attribute::all(),
                )
                .property(
                    js_string!("firstPaintAfterLoadTime"),
                    JsValue::from(0.0),
                    Attribute::all(),
                )
                .property(
                    js_string!("navigationType"),
                    JsValue::from(js_string!("Other")),
                    Attribute::all(),
                )
                .property(
                    js_string!("wasFetchedViaSpdy"),
                    JsValue::from(true),
                    Attribute::all(),
                )
                .property(
                    js_string!("wasNpnNegotiated"),
                    JsValue::from(true),
                    Attribute::all(),
                )
                .build();
            Ok(JsValue::from(o))
        })
    };
    let chrome = ObjectInitializer::new(ctx)
        .property(js_string!("app"), JsValue::from(app), Attribute::all())
        .property(
            js_string!("runtime"),
            JsValue::from(runtime),
            Attribute::all(),
        )
        .function(csi_fn, js_string!("csi"), 0)
        .function(load_times_fn, js_string!("loadTimes"), 0)
        .build();
    JsValue::from(chrome)
}

// ── navigator.plugins + navigator.mimeTypes ──────────────────────────────

/// The 5 plugin entries a real Chrome reports, and the 2 mime types they
/// expose. An empty `navigator.plugins` is the single biggest headless
/// giveaway after `navigator.webdriver`, so these are populated faithfully.
const PLUGIN_NAMES: [(&str, &str); 5] = [
    // (name, description)
    ("PDF Viewer", "Portable Document Format"),
    ("Chrome PDF Viewer", "Portable Document Format"),
    ("Chromium PDF Viewer", "Portable Document Format"),
    ("Microsoft Edge PDF Viewer", "Portable Document Format"),
    ("WebKit built-in PDF", "Portable Document Format"),
];

const MIME_TYPES: [(&str, &str); 2] = [
    // (type, suffixes)
    ("application/pdf", "pdf"),
    ("text/pdf", "pdf"),
];

/// Build `navigator.plugins` and `navigator.mimeTypes` together so the
/// `enabledPlugin` ↔ `MimeType[]` back-references stay coherent.
fn build_plugin_and_mime_arrays(ctx: &mut Context) -> (JsObject, JsObject) {
    // 1. MimeType objects (enabledPlugin linked after plugins exist).
    let mime_objs: Vec<JsObject> = MIME_TYPES
        .iter()
        .map(|(mtype, suffix)| {
            ObjectInitializer::new(ctx)
                .property(
                    js_string!("type"),
                    JsValue::from(js_string!(*mtype)),
                    Attribute::all(),
                )
                .property(
                    js_string!("suffixes"),
                    JsValue::from(js_string!(*suffix)),
                    Attribute::all(),
                )
                .property(
                    js_string!("description"),
                    JsValue::from(js_string!("Portable Document Format")),
                    Attribute::all(),
                )
                .build()
        })
        .collect();

    // 2. Plugin objects, each exposing its mime type at index 0.
    let plugin_objs: Vec<JsObject> = PLUGIN_NAMES
        .iter()
        .enumerate()
        .map(|(i, (name, desc))| {
            let mime = &mime_objs[i.min(mime_objs.len() - 1)];
            ObjectInitializer::new(ctx)
                .property(
                    js_string!("name"),
                    JsValue::from(js_string!(*name)),
                    Attribute::all(),
                )
                .property(
                    js_string!("description"),
                    JsValue::from(js_string!(*desc)),
                    Attribute::all(),
                )
                .property(
                    js_string!("filename"),
                    JsValue::from(js_string!("internal-pdf-viewer")),
                    Attribute::all(),
                )
                .property(js_string!("length"), JsValue::from(1), Attribute::all())
                .property(
                    js_string!("0"),
                    JsValue::from(mime.clone()),
                    Attribute::all(),
                )
                .build()
        })
        .collect();

    // 3. Back-link each mime's enabledPlugin to its owning plugin.
    for (i, mime) in mime_objs.iter().enumerate() {
        let owner = &plugin_objs[i.min(plugin_objs.len() - 1)];
        let _ = mime.set(
            js_string!("enabledPlugin"),
            JsValue::from(owner.clone()),
            true,
            ctx,
        );
    }

    // 4. Assemble the PluginArray (length + indexed + item/namedItem/refresh).
    let plugins_arr = ObjectInitializer::new(ctx)
        .property(
            js_string!("length"),
            JsValue::from(plugin_objs.len() as f64),
            Attribute::all(),
        )
        .build();
    for (i, p) in plugin_objs.iter().enumerate() {
        let _ = plugins_arr.set(
            JsString::from(i.to_string()),
            JsValue::from(p.clone()),
            true,
            ctx,
        );
    }
    let plugins_clone = plugins_arr.clone();
    let _ = plugins_arr.set(
        js_string!("item"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    let idx = args
                        .first()
                        .and_then(|v| v.as_number())
                        .map(|n| n as usize)
                        .unwrap_or(0);
                    Ok(plugin_objs
                        .get(idx)
                        .map(|p| JsValue::from(p.clone()))
                        .unwrap_or(JsValue::null()))
                })
            })
            .name(js_string!("item"))
            .build(),
        ),
        true,
        ctx,
    );
    let _ = plugins_clone.set(
        js_string!("namedItem"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), unsafe {
                NativeFunction::from_closure(move |_this, _args, _ctx| {
                    // Real Chrome resolves by plugin name; null is acceptable
                    // for names that do not match exactly.
                    Ok(JsValue::null())
                })
            })
            .name(js_string!("namedItem"))
            .build(),
        ),
        true,
        ctx,
    );
    let _ = plugins_clone.set(
        js_string!("refresh"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), unsafe {
                NativeFunction::from_closure(|_this, _args, _ctx| Ok(JsValue::undefined()))
            })
            .name(js_string!("refresh"))
            .build(),
        ),
        true,
        ctx,
    );

    // 5. Assemble the MimeTypeArray.
    let mimes_arr = ObjectInitializer::new(ctx)
        .property(
            js_string!("length"),
            JsValue::from(mime_objs.len() as f64),
            Attribute::all(),
        )
        .build();
    for (i, m) in mime_objs.iter().enumerate() {
        let _ = mimes_arr.set(
            JsString::from(i.to_string()),
            JsValue::from(m.clone()),
            true,
            ctx,
        );
    }
    let mimes_clone = mimes_arr.clone();
    // `mime_objs` is moved into the `item` closure below; clone first so the
    // `namedItem` closure can still read it.
    let mime_objs_for_item = mime_objs.clone();
    let _ = mimes_arr.set(
        js_string!("item"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    let idx = args
                        .first()
                        .and_then(|v| v.as_number())
                        .map(|n| n as usize)
                        .unwrap_or(0);
                    Ok(mime_objs_for_item
                        .get(idx)
                        .map(|m| JsValue::from(m.clone()))
                        .unwrap_or(JsValue::null()))
                })
            })
            .name(js_string!("item"))
            .build(),
        ),
        true,
        ctx,
    );
    let _ = mimes_clone.set(
        js_string!("namedItem"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), unsafe {
                NativeFunction::from_closure(move |_this, args, _ctx| {
                    let name = args
                        .first()
                        .and_then(|v| v.as_string())
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default();
                    Ok(MIME_TYPES
                        .iter()
                        .position(|(t, _)| *t == name)
                        .and_then(|i| mime_objs.get(i).cloned())
                        .map(JsValue::from)
                        .unwrap_or(JsValue::null()))
                })
            })
            .name(js_string!("namedItem"))
            .build(),
        ),
        true,
        ctx,
    );

    (plugins_clone, mimes_clone)
}

// ── navigator.userAgentData (Client Hints) ───────────────────────────────

/// `navigator.userAgentData`. `brands` + `mobile` + `platform` are read
/// synchronously by naive detection; `getHighEntropyValues` is the async
/// accessor used by stricter checks.
fn build_user_agent_data(ctx: &mut Context, p: &ChromeProfile) -> JsValue {
    let major_str = p.major.to_string();
    // GREASE brand — a fixed plausible value (real Chrome varies it by
    // version; a static one is indistinguishable to naive detection).
    let brands = build_brands_array(ctx, &major_str);

    let uad = ObjectInitializer::new(ctx)
        .property(js_string!("brands"), brands, Attribute::all())
        .property(js_string!("mobile"), JsValue::from(false), Attribute::all())
        .property(
            js_string!("platform"),
            JsValue::from(js_string!(p.ua_data_platform)),
            Attribute::all(),
        )
        .build();

    // toJSON() → { brands, mobile, platform }
    let uad_for_tojson = uad.clone();
    let to_json_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let b = uad_for_tojson
                .get(js_string!("brands"), ctx)
                .unwrap_or(JsValue::undefined());
            let m = uad_for_tojson
                .get(js_string!("mobile"), ctx)
                .unwrap_or(JsValue::undefined());
            let plat = uad_for_tojson
                .get(js_string!("platform"), ctx)
                .unwrap_or(JsValue::undefined());
            let o = ObjectInitializer::new(ctx)
                .property(js_string!("brands"), b, Attribute::all())
                .property(js_string!("mobile"), m, Attribute::all())
                .property(js_string!("platform"), plat, Attribute::all())
                .build();
            Ok(JsValue::from(o))
        })
    };
    let _ = uad.set(
        js_string!("toJSON"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), to_json_fn)
                .name(js_string!("toJSON"))
                .build(),
        ),
        true,
        ctx,
    );

    // getHighEntropyValues(hints) → thenable resolving to the full payload.
    let major_owned = major_str.clone();
    let full_owned = p.full_version.to_string();
    let plat_owned = p.ua_data_platform.to_string();
    let arch_owned = p.architecture.to_string();
    let bit_owned = p.bitness.to_string();
    let pver_owned = p.platform_version.to_string();
    let uad_for_hev = uad.clone();
    let hev_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let inner = build_high_entropy_payload(
                ctx,
                &uad_for_hev,
                &major_owned,
                &full_owned,
                &plat_owned,
                &arch_owned,
                &bit_owned,
                &pver_owned,
            );
            Ok(make_thenable(ctx, inner))
        })
    };
    let _ = uad.set(
        js_string!("getHighEntropyValues"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), hev_fn)
                .name(js_string!("getHighEntropyValues"))
                .build(),
        ),
        true,
        ctx,
    );

    JsValue::from(uad)
}

/// Build the Sec-CH-UA `brands` list: [GREASE, Chromium, Google Chrome],
/// each at the profile's major version (GREASE at "99").
fn build_brands_array(ctx: &mut Context, major: &str) -> JsValue {
    let mk = |brand: &str, version: &str, ctx: &mut Context| -> JsValue {
        JsValue::from(
            ObjectInitializer::new(ctx)
                .property(
                    js_string!("brand"),
                    JsValue::from(js_string!(brand)),
                    Attribute::all(),
                )
                .property(
                    js_string!("version"),
                    JsValue::from(js_string!(version)),
                    Attribute::all(),
                )
                .build(),
        )
    };
    let arr = boa_engine::object::builtins::JsArray::new(ctx)
        .expect("creating an empty array with the default prototype must not fail");
    let _ = arr.push(mk("Not)A;Brand", "99", ctx), ctx);
    let _ = arr.push(mk("Chromium", major, ctx), ctx);
    let _ = arr.push(mk("Google Chrome", major, ctx), ctx);
    JsValue::from(arr)
}

/// Full high-entropy Client Hints payload (architecture, bitness, full
/// version list, platform version, model).
#[allow(clippy::too_many_arguments)]
fn build_high_entropy_payload(
    ctx: &mut Context,
    uad: &JsObject,
    major: &str,
    full: &str,
    platform: &str,
    architecture: &str,
    bitness: &str,
    platform_version: &str,
) -> JsObject {
    let brands = uad
        .get(js_string!("brands"), ctx)
        .unwrap_or(JsValue::undefined());

    // fullVersionList: same brands but with the full version string.
    let mk = |brand: &str, version: &str, ctx: &mut Context| -> JsValue {
        JsValue::from(
            ObjectInitializer::new(ctx)
                .property(
                    js_string!("brand"),
                    JsValue::from(js_string!(brand)),
                    Attribute::all(),
                )
                .property(
                    js_string!("version"),
                    JsValue::from(js_string!(version)),
                    Attribute::all(),
                )
                .build(),
        )
    };
    let fvl = boa_engine::object::builtins::JsArray::new(ctx)
        .expect("creating an empty array with the default prototype must not fail");
    let _ = fvl.push(mk("Not)A;Brand", "99.0.0.0", ctx), ctx);
    let _ = fvl.push(mk("Chromium", full, ctx), ctx);
    let _ = fvl.push(mk("Google Chrome", full, ctx), ctx);

    ObjectInitializer::new(ctx)
        .property(
            js_string!("architecture"),
            JsValue::from(js_string!(architecture)),
            Attribute::all(),
        )
        .property(
            js_string!("bitness"),
            JsValue::from(js_string!(bitness)),
            Attribute::all(),
        )
        .property(js_string!("brands"), brands, Attribute::all())
        .property(
            js_string!("fullVersionList"),
            JsValue::from(fvl),
            Attribute::all(),
        )
        .property(
            js_string!("fullVersion"),
            JsValue::from(js_string!(full)),
            Attribute::all(),
        )
        .property(js_string!("mobile"), JsValue::from(false), Attribute::all())
        .property(
            js_string!("model"),
            JsValue::from(js_string!("")),
            Attribute::all(),
        )
        .property(
            js_string!("platform"),
            JsValue::from(js_string!(platform)),
            Attribute::all(),
        )
        .property(
            js_string!("platformVersion"),
            JsValue::from(js_string!(platform_version)),
            Attribute::all(),
        )
        .property(
            js_string!("uaFullVersion"),
            JsValue::from(js_string!(full)),
            Attribute::all(),
        )
        .property(
            js_string!("major"),
            JsValue::from(js_string!(major)),
            Attribute::all(),
        )
        .build()
}

// ── navigator.permissions ────────────────────────────────────────────────

/// `navigator.permissions.query({name})`. Real headless Chrome returns
/// `state:"denied"` for notifications; non-headless returns `"prompt"`, so we
/// report `"prompt"`. The result carries `.state` for synchronous reads and a
/// `.then` for `await` / `.then()` consumers (thenable pattern).
fn build_permissions(ctx: &mut Context) -> JsValue {
    let query_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            // Default to "prompt"; real Chrome varies a few names, but "prompt"
            // is the value naive headless-detection looks for as "not headless".
            let name = args
                .first()
                .and_then(|v| v.as_object())
                .and_then(|o| o.get(js_string!("name"), ctx).ok())
                .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
                .unwrap_or_default();
            let state = match name.as_str() {
                // Values a real non-headless Chrome reports.
                "geolocation" | "notifications" | "push" | "persistent-storage" | "midi"
                | "camera" | "microphone" | "background-sync" | "accelerometer" | "gyroscope"
                | "magnetometer" => "prompt",
                _ => "prompt",
            };
            let result = ObjectInitializer::new(ctx)
                .property(
                    js_string!("state"),
                    JsValue::from(js_string!(state)),
                    Attribute::all(),
                )
                .property(js_string!("onchange"), JsValue::null(), Attribute::all())
                .build();
            Ok(make_thenable(ctx, result))
        })
    };
    let perms = ObjectInitializer::new(ctx)
        .function(query_fn, js_string!("query"), 1)
        .build();
    JsValue::from(perms)
}

// ── navigator.connection ─────────────────────────────────────────────────

/// Network Information API values a typical desktop Chrome reports.
fn build_connection(ctx: &mut Context) -> JsValue {
    let conn = ObjectInitializer::new(ctx)
        .property(
            js_string!("effectiveType"),
            JsValue::from(js_string!("4g")),
            Attribute::all(),
        )
        .property(js_string!("rtt"), JsValue::from(50.0), Attribute::all())
        .property(
            js_string!("downlink"),
            JsValue::from(10.0),
            Attribute::all(),
        )
        .property(
            js_string!("saveData"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(
            js_string!("type"),
            JsValue::from(js_string!("wifi")),
            Attribute::all(),
        )
        .build();
    JsValue::from(conn)
}

// ── WebGLRenderingContext / WebGL2RenderingContext ───────────────────────

/// A WebGL context constructor carrying the standard parameter constants on
/// itself and `getParameter`/`getExtension`/`getSupportedExtensions` on its
/// prototype. `typeof WebGLRenderingContext` is then `"function"` (a presence
/// check naive detection performs); a real context object (from
/// `canvas.getContext("webgl")`) is a separate integration point — the
/// prototype methods here are coherent if canvas ever routes to this stub.
fn build_webgl_constructor(ctx: &mut Context, p: &ChromeProfile, is_webgl2: bool) -> JsValue {
    let vendor = p.webgl_vendor.to_string();
    let renderer = p.webgl_renderer.to_string();
    let ctor_name = if is_webgl2 {
        "WebGL2RenderingContext"
    } else {
        "WebGLRenderingContext"
    };
    let version = if is_webgl2 {
        "WebGL 2.0 (OpenGL ES 3.0 Chromium)"
    } else {
        "WebGL 1.0 (OpenGL ES 2.0 Chromium)"
    };
    let sl_version = if is_webgl2 {
        "WebGL GLSL ES 3.00 (OpenGL ES GLSL ES 3.00 Chromium)"
    } else {
        "WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)"
    };
    let version_owned = version.to_string();
    let sl_owned = sl_version.to_string();

    // Constructor body — constructing directly is unsupported (no real GL),
    // so it throws TypeError like a non-constructible builtin in strict use.
    let ctor_fn = NativeFunction::from_copy_closure(|_this, _args, _ctx| Ok(JsValue::undefined()));
    let ctor = FunctionObjectBuilder::new(ctx.realm(), ctor_fn)
        .name(js_string!(ctor_name))
        .build();

    // Parameter-name constants on the constructor.
    let _ = ctor.set(js_string!("VENDOR"), JsValue::from(7936), true, ctx);
    let _ = ctor.set(js_string!("RENDERER"), JsValue::from(7937), true, ctx);
    let _ = ctor.set(js_string!("VERSION"), JsValue::from(7938), true, ctx);
    let _ = ctor.set(
        js_string!("SHADING_LANGUAGE_VERSION"),
        JsValue::from(35724),
        true,
        ctx,
    );
    let _ = ctor.set(
        js_string!("UNMASKED_VENDOR_WEBGL"),
        JsValue::from(37445),
        true,
        ctx,
    );
    let _ = ctor.set(
        js_string!("UNMASKED_RENDERER_WEBGL"),
        JsValue::from(37446),
        true,
        ctx,
    );

    // getParameter(pname) on the prototype.
    let proto = match ctor.get(js_string!("prototype"), ctx) {
        Ok(v) => v.as_object(),
        Err(_) => None,
    }
    // Functions built via FunctionObjectBuilder always carry a prototype;
    // the fallback is defensive only.
    .unwrap_or_else(|| ObjectInitializer::new(ctx).build());

    let vendor_for_get = vendor.clone();
    let renderer_for_get = renderer.clone();
    let version_for_get = version_owned.clone();
    let sl_for_get = sl_owned.clone();
    let get_param_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let pname = args.first().and_then(|v| v.as_number()).unwrap_or(-1.0) as i64;
            let val: JsValue = match pname {
                7936 => JsValue::from(js_string!("WebKit")), // VENDOR
                7937 => JsValue::from(js_string!("WebKit WebGL")), // RENDERER
                7938 => JsValue::from(js_string!(version_for_get.as_str())), // VERSION
                35724 => JsValue::from(js_string!(sl_for_get.as_str())), // SHADING_LANGUAGE_VERSION
                37445 => JsValue::from(js_string!(vendor_for_get.as_str())), // UNMASKED_VENDOR_WEBGL
                37446 => JsValue::from(js_string!(renderer_for_get.as_str())), // UNMASKED_RENDERER_WEBGL
                7939 => JsValue::from(16), // MAX_TEXTURE_SIZE — plausible
                _ => JsValue::null(),
            };
            // Suppress unused-ctx in the no-op branches.
            let _ = ctx;
            Ok(val)
        })
    };
    let _ = proto.set(
        js_string!("getParameter"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), get_param_fn)
                .name(js_string!("getParameter"))
                .build(),
        ),
        true,
        ctx,
    );

    let ext_obj_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            // Only return the debug renderer-info extension (which carries the
            // spoofed UNMASKED_* constants); null otherwise, like real Chrome
            // for unsupported names.
            let name = args
                .first()
                .and_then(|v| v.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            if name == "WEBGL_debug_renderer_info" {
                // Real Chrome returns an opaque extension object; carry the
                // UNMASKED_* constants so `ext.UNMASKED_VENDOR_WEBGL` resolves.
                let ext = ObjectInitializer::new(ctx)
                    .property(
                        js_string!("UNMASKED_VENDOR_WEBGL"),
                        JsValue::from(37445),
                        Attribute::all(),
                    )
                    .property(
                        js_string!("UNMASKED_RENDERER_WEBGL"),
                        JsValue::from(37446),
                        Attribute::all(),
                    )
                    .build();
                Ok(JsValue::from(ext))
            } else {
                Ok(JsValue::null())
            }
        })
    };
    let _ = proto.set(
        js_string!("getExtension"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), ext_obj_fn)
                .name(js_string!("getExtension"))
                .build(),
        ),
        true,
        ctx,
    );

    let supported_fn = unsafe {
        NativeFunction::from_closure(move |_this, _args, ctx| {
            let arr = boa_engine::object::builtins::JsArray::new(ctx)
                .expect("creating an empty array with the default prototype must not fail");
            for ext in [
                "ANGLE_instanced_arrays",
                "EXT_blend_minmax",
                "EXT_color_buffer_half_float",
                "EXT_disjoint_timer_query",
                "EXT_float_blend",
                "EXT_frag_depth",
                "EXT_shader_texture_lod",
                "EXT_texture_compression_bptc",
                "EXT_texture_compression_rgtc",
                "EXT_texture_filter_anisotropic",
                "OES_element_index_uint",
                "OES_fbo_render_mipmap",
                "OES_standard_derivatives",
                "OES_texture_float",
                "OES_texture_float_linear",
                "OES_texture_half_float",
                "OES_texture_half_float_linear",
                "OES_vertex_array_object",
                "WEBGL_color_buffer_float",
                "WEBGL_compressed_texture_s3tc",
                "WEBGL_debug_renderer_info",
                "WEBGL_debug_shaders",
                "WEBGL_depth_texture",
                "WEBGL_draw_buffers",
                "WEBGL_lose_context",
                "WEBGL_multi_draw",
            ]
            .iter()
            {
                let _ = arr.push(JsValue::from(js_string!(*ext)), ctx);
            }
            Ok(JsValue::from(arr))
        })
    };
    let _ = proto.set(
        js_string!("getSupportedExtensions"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), supported_fn)
                .name(js_string!("getSupportedExtensions"))
                .build(),
        ),
        true,
        ctx,
    );

    JsValue::from(ctor)
}

// ── thenable helper ──────────────────────────────────────────────────────

/// Turn `target` into a thenable that resolves to itself: it keeps all of
/// `target`'s own properties (e.g. `.state`) for synchronous reads, and adds
/// a `.then` so `await x` and `Promise.resolve(x)` resolve to it. This covers
/// both `result.state` (sync) and `await navigator.permissions.query(...)`
/// (async) consumers without requiring a full Promise implementation.
fn make_thenable(ctx: &mut Context, target: JsObject) -> JsValue {
    let target_for_then = target.clone();
    let then_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            if let Some(resolve) = args.first().and_then(|v| v.as_object()) {
                let _ = resolve.call(
                    &JsValue::null(),
                    &[JsValue::from(target_for_then.clone())],
                    ctx,
                );
            }
            Ok(JsValue::undefined())
        })
    };
    let _ = target.set(
        js_string!("then"),
        JsValue::from(
            FunctionObjectBuilder::new(ctx.realm(), then_fn)
                .name(js_string!("then"))
                .build(),
        ),
        true,
        ctx,
    );
    JsValue::from(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_major_parses_standard_ua() {
        assert_eq!(
            chrome_major(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.6778.139 Safari/537.36"
            ),
            Some(131)
        );
    }

    #[test]
    fn chrome_major_missing_returns_none() {
        assert_eq!(chrome_major("Mozilla/5.0 Firefox/120.0"), None);
    }

    #[test]
    fn profile_windows_is_direct3d() {
        let p = ChromeProfile::from_ua("... Chrome/131.0 ... Windows NT 10.0 ...");
        assert_eq!(p.platform, "Win32");
        assert_eq!(p.ua_data_platform, "Windows");
        assert!(p.webgl_renderer.contains("Direct3D11"));
        assert!(!p.webgl_renderer.contains("OpenGL 4.1"));
    }

    #[test]
    fn profile_macos_is_opengl_never_direct3d() {
        let p = ChromeProfile::from_ua("... Chrome/131.0 ... Macintosh; Intel Mac OS X ...");
        assert_eq!(p.platform, "MacIntel");
        assert!(p.webgl_renderer.contains("OpenGL 4.1"));
        assert!(!p.webgl_renderer.contains("Direct3D"));
    }

    #[test]
    fn profile_linux() {
        let p = ChromeProfile::from_ua("... Chrome/131.0 ... X11; Linux x86_64 ...");
        assert_eq!(p.platform, "Linux x86_64");
        assert_eq!(p.ua_data_platform, "Linux");
        assert!(p.webgl_renderer.contains("OpenGL 4.6"));
    }

    #[test]
    fn profile_major_singlesource() {
        let p = ChromeProfile::from_ua("... Chrome/120.0.0.0 ...");
        assert_eq!(p.major, 120);
    }
}
