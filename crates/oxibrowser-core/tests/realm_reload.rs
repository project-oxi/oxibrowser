//! Document replacement must refresh Session metadata along with JS realms.
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

#[tokio::test]
async fn reload_recreates_child_frames_and_invalidates_utility_handles() {
    let server = MockServer::start().await;
    Mock::given(path("/app"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<body><iframe id='child' src='/child'></iframe><p id='state'>ready</p></body>",
        ))
        .mount(&server)
        .await;
    Mock::given(path("/child"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<body><b id='inside'>child</b></body>"),
        )
        .mount(&server)
        .await;
    let browser = Browser::new(BrowserConfig {
        enable_ssrf_filter: false,
        ..BrowserConfig::headless()
    })
    .await
    .unwrap();
    let session = browser
        .new_page(&format!("{}/app", server.uri()))
        .await
        .unwrap();
    {
        let mut s = session.write().await;
        assert_eq!(s.frame_context_map().read().len(), 1);
        let world = s
            .create_isolated_world("main", "test-utility")
            .await
            .unwrap();
        let object = s.remote_command(world,json!({"method":"evaluate","params":{"expression":"globalThis.marker='before'; ({alive:true})"}})).await.unwrap();
        let id = object["result"]["objectId"].clone();
        s.reload().await.unwrap();
        assert_eq!(s.frame_context_map().read().len(), 1);
        assert!(s.isolated_worlds().is_empty());
        let fresh = s
            .create_isolated_world("main", "test-utility")
            .await
            .unwrap();
        let marker = s
            .evaluate_js_in_context("typeof marker", fresh, false)
            .await
            .unwrap();
        assert_eq!(marker.value, Some(json!("undefined")));
        assert!(
            s.remote_command(
                fresh,
                json!({"method":"getProperties","params":{"objectId":id}})
            )
            .await
            .is_err()
        );
        let result=s.evaluate_js("document.getElementById('child').contentDocument.getElementById('inside').textContent").await.unwrap();
        assert!(result.exception.is_none(), "{result:?}");
        assert_eq!(result.value, Some(json!("child")));
    }
    browser.close().await.unwrap();
}
