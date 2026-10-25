#![cfg(feature = "webgl")]

use std::borrow::Cow;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use oxibrowser_core::js::CoreEvent;
use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};

const PINNED_REVISION: &str = "3a65272e40f97c82de335825ea77c4da8e77a637";
const SUBSET: &str = include_str!("fixtures/webgl_conformance_subset.txt");

struct StaticServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl StaticServer {
    fn start(root: &Path) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_request(&mut stream, &root),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            stop,
            worker: Some(worker),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.address)
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_request(stream: &mut TcpStream, root: &Path) {
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    loop {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") || request.len() >= 16 * 1024 {
            break;
        }
    }

    let request_text = String::from_utf8_lossy(&request);
    let mut parts = request_text
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let relative = target
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_start_matches('/');
    let decoded = percent_encoding::percent_decode_str(relative).decode_utf8_lossy();
    let resolved = root.join(decoded.as_ref()).canonicalize().ok();
    let file = resolved
        .filter(|path| path.starts_with(root))
        .and_then(|path| std::fs::read(path).ok());

    let (status, content_type, body): (&str, &str, Cow<'static, [u8]>) = if method != "GET" {
        (
            "405 Method Not Allowed",
            "text/plain",
            Cow::Borrowed(b"method not allowed"),
        )
    } else if let Some(body) = file {
        let content_type = match Path::new(relative).extension().and_then(|ext| ext.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "application/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("json") => "application/json",
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            _ => "application/octet-stream",
        };
        ("200 OK", content_type, Cow::Owned(body))
    } else {
        ("404 Not Found", "text/plain", Cow::Borrowed(b"not found"))
    };
    let body = body.as_ref();

    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn suite_revision(repo: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args([
            "-C",
            repo.to_str().ok_or("non-UTF-8 suite path")?,
            "rev-parse",
            "HEAD",
        ])
        .output()
        .map_err(|error| format!("run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn parse_results(text: &str) -> Option<(usize, usize)> {
    let summary = text.rsplit("TEST COMPLETE:").next()?;
    let (passes, after_passes) = summary.split_once(" PASS,")?;
    let passes = passes.split_whitespace().last()?.parse().ok()?;
    let (failures, _) = after_passes.trim_start().split_once(" FAIL")?;
    let failures = failures.split_whitespace().next()?.parse().ok()?;
    Some((passes, failures))
}

async fn completed_summary(session: &mut Session) -> Result<String, String> {
    let mut last_text = String::new();
    for _ in 0..200 {
        let result = session
            .evaluate_js(
                "(document.getElementById('console') && document.getElementById('console').textContent || '') + '\\n' + ((document.getElementById('description') && document.getElementById('description').textContent) || '')",
            )
            .await
            .map_err(|error| format!("evaluate result DOM: {error}"))?;
        if let Some(error) = result.exception {
            return Err(format!("result DOM evaluation threw: {error}"));
        }
        last_text = result
            .value
            .as_ref()
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        if last_text.contains("TEST COMPLETE:") {
            return Ok(last_text);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let diagnostics = session
        .evaluate_js(
            "JSON.stringify({href: location.href, readyState: document.readyState, title: document.title, parent: typeof window.parent, parentIsWindow: window.parent === window, glCtorWindow: typeof window.WebGLRenderingContext, glCtorGlobal: typeof WebGLRenderingContext, glRgba: WebGLRenderingContext.RGBA, glPoints: WebGLRenderingContext.POINTS, objectEntries: typeof Object.entries, objectDefineProperties: typeof Object.defineProperties, description: typeof description, windowDescription: typeof window.description, shouldBeTrue: typeof shouldBeTrue, successfullyParsed: typeof successfullyParsed, testPassed: typeof testPassed, webglTestUtils: typeof WebGLTestUtils, results: typeof RESULTS, descriptionText: document.getElementById('description').textContent, descriptionChildren: document.getElementById('description').childNodes.length, scriptSources: Array.prototype.map.call(document.getElementsByTagName('script'), function(script) { return script.src; }).join('|'), bodyText: document.body.textContent, scripts: document.scripts && document.scripts.length})",
        )
        .await
        .map(|result| format!("value={:?}, exception={:?}", result.value, result.exception))
        .unwrap_or_else(|error| format!("evaluation error={error}"));
    Err(format!(
        "no TEST COMPLETE marker within 10s; description={last_text:?}; runtime={diagnostics}"
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the pinned Khronos checkout and ANGLE libraries; run scripts/run-webgl-conformance.sh"]
async fn pinned_khronos_webgl_subset_reports_harness_results() {
    let repo = std::env::var_os("WEBGL_CONFORMANCE_REPO")
        .map(PathBuf::from)
        .expect("set WEBGL_CONFORMANCE_REPO by running scripts/run-webgl-conformance.sh");
    let actual_revision = suite_revision(&repo).expect("read suite revision");
    assert_eq!(
        actual_revision, PINNED_REVISION,
        "wrong Khronos WebGL revision"
    );

    let tests_root = repo.join("sdk/tests");
    assert!(
        tests_root.join("js/js-test-pre.js").is_file(),
        "missing Khronos harness"
    );
    let tests: Vec<&str> = SUBSET
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("revision="))
        .collect();
    assert!(!tests.is_empty(), "empty Khronos subset manifest");

    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.expect("browser");
    let mut failures = Vec::new();

    for path in tests.iter().copied() {
        let page = tests_root.join(path);
        assert!(
            page.is_file(),
            "missing conformance page: {}",
            page.display()
        );
        let server = StaticServer::start(&tests_root).expect("start conformance file server");
        let mut session = Session::new(browser.id(), config.clone(), browser.default_context())
            .await
            .expect("create isolated test session");
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        session.set_event_sink(event_tx);
        let url = server.url(path);
        let outcome = async {
            session
                .navigate(&url)
                .await
                .map_err(|error| format!("navigate: {error}"))?;
            let summary = completed_summary(&mut session).await?;
            let (passed, failed) = parse_results(&summary)
                .ok_or_else(|| format!("malformed test summary: {summary}"))?;
            Ok::<_, String>((passed, failed, summary))
        }
        .await;
        let script_errors = event_rx
            .try_iter()
            .filter_map(|event| match event {
                CoreEvent::Exception {
                    message,
                    name,
                    stack,
                    ..
                } => Some(format!("{name}: {message}; stack={stack:?}")),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !script_errors.is_empty() {
            println!("JS exceptions {path}: {}", script_errors.join(" | "));
        }

        match outcome {
            Ok((passed, failed, _summary)) if passed > 0 && failed == 0 => {
                println!("PASS {path}: {passed} passed, {failed} failed");
            }
            Ok((passed, failed, summary)) => {
                println!("FAIL {path}: {passed} passed, {failed} failed; {summary}");
                failures.push(format!("{path}: {summary}"));
            }
            Err(detail) => {
                println!("FAIL {path}: {detail}");
                failures.push(format!("{path}: {detail}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "Khronos conformance failures:\n{}",
        failures.join("\n")
    );
    println!("Khronos WebGL subset: {} PASS, 0 FAIL", tests.len());
}
