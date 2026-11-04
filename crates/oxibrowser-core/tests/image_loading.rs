use base64::Engine;
use oxibrowser_core::js::runtime::{FetchRequestMsg, FetchResponseMsg};
use oxibrowser_core::js::{JsRuntime, JsRuntimeConfig};
use serde_json::{Value, json};
use std::sync::mpsc;
use std::time::Duration;

const PIXELS: [u8; 8] = [255, 0, 0, 255, 0, 255, 0, 128];

fn png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(pixels, width, height, image::ExtendedColorType::Rgba8)
        .unwrap();
    bytes
}

fn source() -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png(2, 1, &PIXELS))
    )
}

async fn runtime() -> JsRuntime {
    let mut runtime = JsRuntime::with_config(JsRuntimeConfig {
        background_event_loop: true,
        ..Default::default()
    });
    runtime.set_page_url("https://image.example/page");
    runtime
        .set_document(
            "<html><body><p id='state'>initial</p></body></html>",
            Some("https://image.example/page"),
            (320, 240),
        )
        .await
        .unwrap();
    runtime
}

async fn evaluate(runtime: &mut JsRuntime, expression: &str) -> Value {
    let result = runtime.evaluate(expression).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    result.value.unwrap_or(Value::Null)
}

async fn value(runtime: &mut JsRuntime, expression: &str) -> Value {
    let result = evaluate(runtime, &format!("JSON.stringify({expression})")).await;
    serde_json::from_str(result.as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn png_data_url_decodes_real_pixels_and_delivers_ordered_async_events() {
    let mut runtime = runtime().await;
    let source = serde_json::to_string(&source()).unwrap();
    evaluate(&mut runtime,&format!(r#"
      globalThis.img = new Image(7,9); globalThis.order = [];
      img.addEventListener('load', function(event) {{ order.push(['listener',this === img,event.target === img,img.complete,img.naturalWidth,img.naturalHeight]); }});
      img.onload = () => order.push(['property']);
      img.src = {source}; order.push(['assigned',img.complete]);
      img.decode().then(() => order.push(['decoded']));
    "#)).await;
    assert_eq!(value(&mut runtime,"[order,img.width,img.height,img instanceof Image,img.tagName,Array.from(new Uint8Array(__oxiImagePixels(img).data))]").await,
        json!([[ ["assigned",false],["listener",true,true,true,2,1],["property"],["decoded"] ],7,9,true,"IMG",PIXELS]));
    evaluate(
        &mut runtime,
        "new Uint8Array(__oxiImagePixels(img).data)[0] = 0",
    )
    .await;
    assert_eq!(
        value(
            &mut runtime,
            "new Uint8Array(__oxiImagePixels(img).data)[0]"
        )
        .await,
        json!(255)
    );
}

#[tokio::test]
async fn invalid_image_bytes_emit_one_error_and_never_a_load() {
    let mut runtime = runtime().await;
    evaluate(&mut runtime,r#"
      globalThis.bad = document.createElement('img'); globalThis.events=[];
      bad.onerror = function(event) { events.push(['error',this === bad,event.target === bad,bad.complete,bad.naturalWidth]); };
      bad.onload = () => events.push(['unexpected load']);
      bad.src='data:image/png;base64,bm90IGEgcG5n';
      bad.decode().catch(error => events.push(['decode',error.name]));
    "#).await;
    assert_eq!(value(&mut runtime,"[events, (()=>{try{__oxiImagePixels(bad)}catch(e){return e.name}})(),__oxiImagePixels({})]").await,
        json!([[ ["error",true,true,true,0],["decode","EncodingError"] ],"InvalidStateError",null]));
    assert_eq!(value(&mut runtime, "events.length").await, json!(2));
}

#[tokio::test]
async fn source_replacement_cancels_prior_decode_and_its_events() {
    let mut runtime = runtime().await;
    let source = serde_json::to_string(&source()).unwrap();
    evaluate(
        &mut runtime,
        &format!(
            r#"
      globalThis.img = new Image(); globalThis.events=[];
      img.onload = () => events.push('load'); img.onerror = () => events.push('error');
      img.src='data:image/png;base64,bm90IGEgcG5n';
      img.decode().catch(error => events.push(error.name));
      img.src={source}; img.decode().then(() => events.push('decoded'));
    "#
        ),
    )
    .await;
    assert_eq!(
        value(&mut runtime, "[events,img.complete,img.naturalWidth]").await,
        json!([["EncodingError", "load", "decoded"], true, 2])
    );
}

#[tokio::test]
async fn blob_urls_require_real_blobs_and_revocation_preserves_already_decoded_pixels() {
    let mut runtime = runtime().await;
    let bytes = serde_json::to_string(&png(2, 1, &PIXELS)).unwrap();
    evaluate(&mut runtime,&format!(r#"
      globalThis.img = new Image(); globalThis.events=[];
      globalThis.url=URL.createObjectURL(new Blob([new Uint8Array({bytes})],{{type:'image/png'}}));
      img.addEventListener('load', function() {{ events.push(Array.from(new Uint8Array(__oxiImagePixels(img).data))); URL.revokeObjectURL(img.src); }},{{once:true}});
      img.src=url;
    "#)).await;
    assert_eq!(value(&mut runtime,"[events,Array.from(new Uint8Array(__oxiImagePixels(img).data)),url.startsWith('blob:https://image.example/'),(()=>{try{URL.createObjectURL({__isBlob:true,__bytes:new Uint8Array([1])})}catch(e){return e.name}})()]").await,
        json!([[PIXELS],PIXELS,true,"TypeError"]));
    evaluate(&mut runtime,"globalThis.revoked=new Image(); revoked.onerror=()=>events.push('revoked'); revoked.src=url; revoked.decode().catch(e=>events.push(e.name)); URL.revokeObjectURL(url)").await;
    assert_eq!(
        value(
            &mut runtime,
            "[events.slice(1),img.naturalWidth,revoked.naturalWidth]"
        )
        .await,
        json!([["revoked", "EncodingError"], 2, 0])
    );
}

#[tokio::test]
async fn strict_decoder_dimensions_reject_oversized_png_without_publishing_pixels() {
    let mut runtime = runtime().await;
    let oversized = png(16_385, 1, &vec![0; 16_385 * 4]);
    let source = serde_json::to_string(&format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(oversized)
    ))
    .unwrap();
    evaluate(&mut runtime,&format!("globalThis.img=new Image();globalThis.events=[];img.onload=()=>events.push('load');img.onerror=()=>events.push('error');img.src={source};img.decode().catch(e=>events.push(e.name))")).await;
    assert_eq!(
        value(&mut runtime, "[events,img.naturalWidth,img.naturalHeight]").await,
        json!([["error", "EncodingError"], 0, 0])
    );
}

fn bridge(
    runtime: &mut JsRuntime,
) -> (
    mpsc::Receiver<FetchRequestMsg>,
    mpsc::Sender<FetchResponseMsg>,
) {
    let (requests, received) = mpsc::channel();
    let (responses, pending) = mpsc::channel();
    runtime.set_fetch_channel(requests, pending);
    (received, responses)
}

#[tokio::test]
async fn navigation_discards_pending_image_response_callbacks_and_blob_urls() {
    let mut runtime = runtime().await;
    let (requests, responses) = bridge(&mut runtime);
    evaluate(&mut runtime,"globalThis.img=new Image();globalThis.events=[];globalThis.url=URL.createObjectURL(new Blob([]));img.onload=()=>document.querySelector('#state').textContent='stale';setTimeout(()=>{img.src='/late.png';img.decode().catch(()=>{})},60)").await;
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request.origin.as_deref(), Some("https://image.example"));
    runtime
        .set_document(
            "<html><body><p id='state'>new</p></body></html>",
            Some("https://image.example/new"),
            (320, 240),
        )
        .await
        .unwrap();
    responses
        .send(FetchResponseMsg {
            id: request.id,
            status: 200,
            status_text: "OK".into(),
            url: request.url,
            headers: vec![],
            body: String::new(),
            body_bytes: Some(png(2, 1, &PIXELS)),
            error: None,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runtime.query_selector_all("#state").await.unwrap()[0].text,
        "new"
    );
    evaluate(
        &mut runtime,
        "globalThis.next=new Image();next.onerror=()=>events.push('revoked');next.src=url",
    )
    .await;
    assert_eq!(
        value(
            &mut runtime,
            "[events,(()=>{try{__oxiImagePixels(img)}catch(e){return e.name}})()]"
        )
        .await,
        json!([["revoked"], "InvalidStateError"])
    );
}

#[tokio::test]
async fn cross_origin_image_loading_is_explicitly_unsupported_without_sending_fetch() {
    let mut runtime = runtime().await;
    let (requests, _responses) = bridge(&mut runtime);
    evaluate(&mut runtime,"globalThis.img=new Image();globalThis.events=[];img.onerror=()=>events.push('error');img.src='https://other.example/pixel.png';img.decode().catch(e=>events.push(e.name))").await;
    assert_eq!(
        value(&mut runtime, "events").await,
        json!(["error", "EncodingError"])
    );
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn replacing_an_inflight_http_image_ignores_its_late_response() {
    let mut runtime = runtime().await;
    let (requests, responses) = bridge(&mut runtime);
    evaluate(&mut runtime,"globalThis.img=new Image();globalThis.events=[];img.onload=()=>events.push('load');img.onerror=()=>events.push('error');setTimeout(()=>{img.src='/slow.png';img.decode().catch(error=>events.push(error.name))},60)").await;
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let source = serde_json::to_string(&source()).unwrap();
    evaluate(
        &mut runtime,
        &format!("img.src={source};img.decode().then(()=>events.push('decoded'))"),
    )
    .await;
    responses
        .send(FetchResponseMsg {
            id: request.id,
            status: 200,
            status_text: "OK".into(),
            url: request.url,
            headers: vec![],
            body: String::new(),
            body_bytes: Some(png(1, 1, &[0, 0, 255, 255])),
            error: None,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        value(
            &mut runtime,
            "[events,img.naturalWidth,Array.from(new Uint8Array(__oxiImagePixels(img).data))]"
        )
        .await,
        json!([["EncodingError", "load", "decoded"], 2, PIXELS])
    );
}

#[tokio::test]
async fn same_origin_http_image_uses_real_fetch_bytes_and_decodes_png() {
    use oxibrowser_core::session::Session;
    use oxibrowser_core::{Browser, BrowserConfig};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html><body></body></html>", "text/html"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pixels.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(png(2, 1, &PIXELS), "image/png"))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let result = session.evaluate_js_with_await("(()=>{const image=new Image();image.src='/pixels.png';return image.decode().then(()=>JSON.stringify([image.complete,image.naturalWidth,image.naturalHeight,Array.from(new Uint8Array(__oxiImagePixels(image).data)),window.Image===Image]));})()",true).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    let result: Value =
        serde_json::from_str(result.value.as_ref().and_then(Value::as_str).unwrap()).unwrap();
    assert_eq!(result, json!([true, 2, 1, PIXELS, true]));
    server.verify().await;
}

#[tokio::test]
async fn invalid_url_reflects_authored_src_without_retaining_old_native_source() {
    let mut runtime = runtime().await;
    let source = serde_json::to_string(&source()).unwrap();
    evaluate(
        &mut runtime,
        &format!("globalThis.img=new Image();img.src={source};document.body.appendChild(img)"),
    )
    .await;
    evaluate(&mut runtime,"globalThis.errors=0;img.onerror=()=>errors++;img.src='http://[';img.decode().catch(()=>{})").await;
    assert_eq!(value(&mut runtime,"[img.src,img.getAttribute('src'),img.hasAttribute('src'),img.complete,img.naturalWidth,errors]").await,
        json!(["http://[","http://[",true,true,0,1]));
    evaluate(&mut runtime, "img.removeAttribute('src')").await;
    assert_eq!(
        value(
            &mut runtime,
            "[img.src,img.getAttribute('src'),img.hasAttribute('src')]"
        )
        .await,
        json!(["", null, false])
    );
}
