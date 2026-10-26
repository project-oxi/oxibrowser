//! Concurrent HTTP module graphs, observed through live DOM snapshots, never eval.
//!
//! Generic fixtures use automatically assigned loopback ports. Ignored probes
//! require OXI_CHEONGGYE_ASSETS pointing at unchanged retained build assets and
//! exclusively bind port 19229. Every server stops on drop; no GPU or game boot.

use oxibrowser_core::js::CoreEvent;
use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

static PORT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static DIAGNOSTICS: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();

struct Diagnostics(Arc<Mutex<Vec<String>>>);
impl tracing::Subscriber for Diagnostics {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target().ends_with("job_queue")
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                let _ = write!(self.0, "{}={value:?} ", field.name());
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

fn diagnostics() -> Vec<String> {
    DIAGNOSTICS
        .get()
        .map(|log| log.lock().unwrap().clone())
        .unwrap_or_default()
}

fn start_diagnostics() {
    let log = DIAGNOSTICS.get_or_init(|| {
        let log = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Diagnostics(log.clone())).unwrap();
        log
    });
    log.lock().unwrap().clear();
}

async fn server() -> MockServer {
    start_diagnostics();
    MockServer::start().await
}

async fn serve(server: &MockServer, route: &str, mime: &str, body: impl AsRef<[u8]>) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.as_ref(), mime))
        .mount(server)
        .await;
}

async fn session() -> (Session, Receiver<CoreEvent>) {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    config.background_event_loop = true;
    config.nav_script_timeout_ms = 30_000;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    session.set_event_sink(tx);
    (session, rx)
}

async fn state(session: &mut Session) -> Option<Value> {
    let snapshot = session.dom_snapshot().await.unwrap()?;
    let node = snapshot.query_selector("#state")?;
    let text = snapshot.text_content(node)?;
    serde_json::from_str(&text).ok()
}

async fn finished(
    session: &mut Session,
    events: &Receiver<CoreEvent>,
    server: &MockServer,
) -> Value {
    let start = Instant::now();
    loop {
        if let Some(value) = state(session).await
            && value["status"] != "pending"
        {
            let exceptions: Vec<_> = events
                .try_iter()
                .filter_map(|event| match event {
                    CoreEvent::Exception { message, stack, .. } => Some((message, stack)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                value["status"], "done",
                "DOM state: {value}; exceptions: {exceptions:?}"
            );
            assert!(exceptions.is_empty(), "exceptions: {exceptions:?}");
            return value;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let requests: Vec<_> = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|r| r.url.path().to_string())
                .collect();
            let exceptions: Vec<_> = events
                .try_iter()
                .filter_map(|event| match event {
                    CoreEvent::Exception { message, stack, .. } => Some((message, stack)),
                    _ => None,
                })
                .collect();
            panic!(
                "module graph stalled: state={:?}; requests={requests:?}; exceptions={exceptions:?}; queue={:?}",
                state(session).await,
                diagnostics()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn page(script: &str) -> String {
    format!(
        r#"<!doctype html><html><body><pre id="state">{{"status":"pending"}}</pre><script type="module">
        const report=value=>document.getElementById('state').textContent=JSON.stringify(value);
        const run=async()=>{{{script}}};
        setTimeout(()=>run().catch(error=>report({{status:'error',message:String(error),stack:error.stack}})),25);
    </script></body></html>"#
    )
}

#[tokio::test]
async fn many_concurrent_graphs_share_cached_dependencies_over_http() {
    graph_case(32, 4, false).await;
}

#[tokio::test]
async fn concurrent_dynamic_imports_of_top_level_await_modules() {
    graph_case(32, 4, true).await;
}

async fn graph_case(roots: usize, repeats: usize, tla: bool) {
    let _port = PORT.lock().await;
    let server = server().await;
    serve(
        &server,
        "/shared.js",
        "text/javascript",
        r#"
        globalThis.sharedRuns=(globalThis.sharedRuns||0)+1;
        export const token={};export const seen={leaves:[],async:[],entries:[],runs:0};
    "#,
    )
    .await;
    for index in 0..8 {
        serve(&server,&format!("/graph/leaf-{index}.js"),"text/javascript",format!(
            "import {{token,seen}} from '../shared.js';seen.leaves.push({index});export {{token}};"
        )).await;
        serve(&server,&format!("/graph/async-{index}.js"),"text/javascript",format!(
            "import {{token,seen}} from '../shared.js';{};seen.async.push({index});export {{token}};",if tla {"await Promise.resolve()"} else {""}
        )).await;
    }
    for index in 0..roots {
        let a = index % 8;
        let b = (index + 1) % 8;
        serve(&server,&format!("/graph/branch-{index}.js"),"text/javascript",format!(
            "import {{token as a}} from './leaf-{a}.js';import {{token as b}} from './leaf-{b}.js';export const same=a===b;"
        )).await;
        serve(&server,&format!("/graph/entry-{index}.js"),"text/javascript",format!(r#"
            import {{same}} from './branch-{index}.js';import {{token,seen}} from '../shared.js';
            seen.entries.push({index});
            export async function run() {{
                await Promise.resolve();
                const [a,b]=await Promise.all([import('./async-{a}.js'),import('../shared.js')]);
                seen.runs++;
                document.getElementById('state').textContent=JSON.stringify({{status:'pending',phase:'runs',runs:seen.runs,async:seen.async.length}});
                return same && a.token===token && b.token===token;
            }}
        "#)).await;
    }
    let imports = roots * repeats;
    let source = page(&format!(
        r#"
        const urls=Array.from({{length:{imports}}},(_,i)=>'/graph/entry-'+(i%{roots})+'.js');
        const modules=await Promise.all(urls.map(url=>import(url)));
        report({{status:'pending',phase:'roots imported',imports:modules.length}});
        const values=await Promise.all(modules.map(module=>module.run()));
        const shared=await import('/shared.js');
        report({{status:'done',imports:modules.length,passed:values.filter(Boolean).length,
            cached:modules[0]===modules[{roots}],sharedRuns,
            leaves:shared.seen.leaves.length,async:shared.seen.async.length,entries:shared.seen.entries.length}});
    "#
    ));
    serve(&server, "/page", "text/html", source).await;
    let (mut session, events) = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(
        finished(&mut session, &events, &server).await,
        json!({
            "status":"done","imports":imports,"passed":imports,"cached":true,"sharedRuns":1,
            "leaves":(roots+1).min(8),"async":roots.min(8),"entries":roots
        })
    );
    let mut counts = BTreeMap::new();
    for request in server.received_requests().await.unwrap() {
        *counts
            .entry(request.url.path().to_owned())
            .or_insert(0usize) += 1;
    }
    assert_eq!(
        counts.len(),
        2 + 2 * roots + (roots + 1).min(8) + roots.min(8),
        "HTTP records: {counts:?}"
    );
    assert!(
        counts.values().all(|count| *count == 1),
        "cache misses: {counts:?}"
    );
    assert!(
        !diagnostics().iter().any(|line| line.contains("dropping")),
        "lost continuations: {:?}",
        diagnostics()
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn ordinary_promise_storm_preserves_work_and_context_responds() {
    let _port = PORT.lock().await;
    let server = server().await;
    let source = page(
        r#"
        let count=0;
        function churn() {
            count++;
            if(count===25000) report({status:'done',count});
            else Promise.resolve().then(churn);
        }
        Promise.resolve().then(churn);
    "#,
    );
    serve(&server, "/page", "text/html", source).await;
    serve(
        &server,
        "/next",
        "text/html",
        r#"<pre id="state">{"status":"done","stage":"next"}</pre>"#,
    )
    .await;
    let (mut session, events) = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), session.dom_snapshot())
        .await
        .expect("a pure promise workload must yield to context commands")
        .unwrap();
    assert_eq!(
        finished(&mut session, &events, &server).await,
        json!({"status":"done","count":25000})
    );
    session
        .navigate(&format!("{}/next", server.uri()))
        .await
        .unwrap();
    assert_eq!(
        state(&mut session).await,
        Some(json!({"status":"done","stage":"next"}))
    );
    session.close().await.unwrap();
}

const SHADER_CHUNKS: &[&str] = &[
    "clipPlaneFragment-DVK0wgyZ.js",
    "packingFunctions-DpGwbupU.js",
    "bakedVertexAnimation-C6l_KSSy.js",
    "clipPlaneVertex-C59BO8JS.js",
    "sceneUboDeclaration-B5VhSG0v.js",
    "meshUboDeclaration-BmNu2KU_.js",
    "morphTargetsVertex-6N5r7oF1.js",
    "sceneVertexDeclaration-CluTp-RC.js",
];

async fn retained_server() -> MockServer {
    let root = PathBuf::from(
        std::env::var_os("OXI_CHEONGGYE_ASSETS")
            .expect("set OXI_CHEONGGYE_ASSETS to the retained asset directory"),
    );
    assert!(
        root.join(SHADER_CHUNKS[0]).is_file(),
        "retained asset root missing"
    );
    start_diagnostics();
    let server = MockServer::builder()
        .listener(
            TcpListener::bind("127.0.0.1:19229").expect("exclusive retained probe port 19229"),
        )
        .start()
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/assets/[^/]+\.js$"))
        .respond_with(move |request: &Request| {
            let name = request.url.path().strip_prefix("/assets/").unwrap();
            match std::fs::read(root.join(name)) {
                Ok(bytes) => ResponseTemplate::new(200).set_body_raw(bytes, "text/javascript"),
                Err(_) => ResponseTemplate::new(404),
            }
        })
        .mount(&server)
        .await;
    server
}

#[tokio::test]
#[ignore = "manual retained production assets; no GPU or game boot"]
async fn retained_shader_chunks_finish_concurrent_imports_without_gpu() {
    let _port = PORT.lock().await;
    let server = retained_server().await;
    let chunks = serde_json::to_string(SHADER_CHUNKS).unwrap();
    let source = page(&format!(
        r#"
        const chunks={chunks};
        const modules=await Promise.all(Array.from({{length:128}},(_,i)=>import('/assets/'+chunks[i%chunks.length])));
        const {{t:store}}=await import('/assets/shaderStore-D-XQlhUT.js');
        report({{status:'done',imports:modules.length,cached:modules[0]===modules[8],
            includes:Object.keys(store.IncludesShadersStore).sort()}});
    "#
    ));
    serve(&server, "/page", "text/html", source).await;
    let (mut session, events) = session().await;
    let start = Instant::now();
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let result = finished(&mut session, &events, &server).await;
    assert_eq!(result["imports"], 128);
    assert_eq!(result["cached"], true);
    assert!(
        result["includes"].as_array().unwrap().len() >= 8,
        "{result}"
    );
    let requests: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    eprintln!(
        "retained shader imports PASS in {:?}; state={result}; requests={requests:?}",
        start.elapsed()
    );
    assert_eq!(
        requests.len(),
        10,
        "one page + eight shader modules + shared store"
    );
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "manual retained shader/glTF graphs; imports runtime but never boots game or GPU"]
async fn retained_shadow_shaders_and_gltf_extension_graphs() {
    let _port = PORT.lock().await;
    let server = retained_server().await;
    let source = page(
        r#"
        await import('/assets/runtime-BBlrYGBF.js');
        report({status:'pending',phase:'production runtime cached'});
        const roots=['shadowMap.fragment-DX4Yo1bp.js','shadowMap.vertex-CNA2cQYs.js','depthBoxBlur.fragment-CLEAG5DG.js',
            'KHR_materials_clearcoat.pure-DofRvg3o.js','KHR_materials_specular.pure-xDDNcevQ.js','KHR_materials_ior.pure-R75ETrPg.js'];
        const modules=await Promise.all(Array.from({length:48},(_,i)=>import('/assets/'+roots[i%roots.length])));
        const {t:store}=await import('/assets/shaderStore-D-XQlhUT.js');
        report({status:'done',imports:modules.length,cached:modules[0]===modules[6],
            shaders:Object.keys(store.ShadersStore).sort(),includes:Object.keys(store.IncludesShadersStore).sort()});
    "#,
    );
    serve(&server, "/page", "text/html", source).await;
    let (mut session, events) = session().await;
    let start = Instant::now();
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let result = finished(&mut session, &events, &server).await;
    assert_eq!(result["imports"], 48);
    assert_eq!(result["cached"], true);
    assert!(
        result["shaders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "shadowMapVertexShader"),
        "{result}"
    );
    let requests: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    eprintln!(
        "retained shadow/glTF graphs PASS in {:?}; state={result}; {}requests; queue={:?}",
        start.elapsed(),
        requests.len(),
        diagnostics()
    );
    session.close().await.unwrap();
}
