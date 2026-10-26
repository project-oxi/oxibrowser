#![cfg(feature = "webgl")]

use std::io::Cursor;

use base64::Engine as _;
use oxibrowser_core::{Browser, BrowserConfig, session::Session};
use serde_json::json;

#[tokio::test]
async fn decoded_image_uploads_use_real_dimensions_pixels_and_unpack_state() {
    let pixels: Vec<u8> = vec![
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 255, 255, 255, 255, 0, 255, 255, 200,
        100, 50, 128,
    ];
    let image = image::RgbaImage::from_raw(3, 2, pixels.clone()).unwrap();
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let source = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(encoded.into_inner())
    );
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate("data:text/html,<canvas id='canvas'></canvas>")
        .await
        .unwrap();
    let expression = format!(
        r#"(async () => {{
        const source = {source};
        const bytes = Uint8Array.from(atob(source.split(',')[1]), c => c.charCodeAt(0));
        const url = URL.createObjectURL(new Blob([bytes], {{type:'image/png'}}));
        const image = new Image();
        image.src = url;
        await image.decode();
        URL.revokeObjectURL(url);
        const gl = document.getElementById('canvas').getContext('webgl');
        const texture = gl.createTexture(); gl.bindTexture(gl.TEXTURE_2D, texture);
        gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MIN_FILTER,gl.NEAREST);
        gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MAG_FILTER,gl.NEAREST);
        gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_WRAP_S,gl.CLAMP_TO_EDGE);
        gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_WRAP_T,gl.CLAMP_TO_EDGE);
        gl.pixelStorei(gl.UNPACK_ALIGNMENT,8);
        gl.texImage2D(gl.TEXTURE_2D,0,gl.RGBA,gl.RGBA,gl.UNSIGNED_BYTE,image);
        const framebuffer = gl.createFramebuffer(); gl.bindFramebuffer(gl.FRAMEBUFFER,framebuffer);
        gl.framebufferTexture2D(gl.FRAMEBUFFER,gl.COLOR_ATTACHMENT0,gl.TEXTURE_2D,texture,0);
        if(gl.checkFramebufferStatus(gl.FRAMEBUFFER)!==gl.FRAMEBUFFER_COMPLETE) throw new Error('incomplete framebuffer');
        const read = () => {{ const pixels = new Uint8Array(24); gl.readPixels(0,0,3,2,gl.RGBA,gl.UNSIGNED_BYTE,pixels); return Array.from(pixels); }};
        const first = read();
        gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL,true);
        gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL,true);
        gl.texSubImage2D(gl.TEXTURE_2D,0,0,0,gl.RGBA,gl.UNSIGNED_BYTE,image);
        const transformed = read();
        gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL,false);
        gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL,false);
        gl.texImage2D(gl.TEXTURE_2D,0,gl.RGB,gl.RGB,gl.UNSIGNED_BYTE,image);
        return JSON.stringify({{first,transformed,rgb:read(),alignment:gl.getParameter(gl.UNPACK_ALIGNMENT),error:gl.getError()}});
    }})()"#,
        source = serde_json::to_string(&source).unwrap()
    );
    let result = session
        .evaluate_js_with_await(&expression, true)
        .await
        .unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    let value: serde_json::Value = serde_json::from_str(
        result
            .value
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .expect("image upload result"),
    )
    .unwrap_or_else(|error| panic!("{error}: {result:?}"));
    let transformed = vec![
        0, 255, 255, 255, 255, 0, 255, 255, 100, 50, 25, 128, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0,
        255, 255,
    ];
    let mut rgb = pixels.clone();
    rgb[23] = 255;
    assert_eq!(
        value,
        json!({"first":pixels,"transformed":transformed,"rgb":rgb,"alignment":8,"error":0})
    );
}
