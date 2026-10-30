//! Idle progress is observed through render queries or bridge messages. Those
//! commands do not pump JS; evaluation is used only to schedule initial work.

use std::sync::mpsc;
use std::time::Duration;

use oxibrowser_core::js::runtime::{FetchRequestMsg, FetchResponseMsg};
use oxibrowser_core::js::{DomMutation, JsRuntime, JsRuntimeConfig};

const HTML: &str = "<html><body><div id='state'>initial</div></body></html>";

async fn runtime(enabled: bool) -> JsRuntime {
    let mut rt = JsRuntime::with_config(JsRuntimeConfig {
        background_event_loop: enabled,
        nav_script_max_loop_iterations: 10_000,
        ..Default::default()
    });
    rt.set_page_url("https://main.example/page");
    rt.set_document(HTML, Some("https://main.example/page"), (640, 480))
        .await
        .unwrap();
    rt
}

async fn eval(rt: &mut JsRuntime, source: &str) {
    let result = rt.evaluate(source).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
}

async fn text(rt: &mut JsRuntime) -> String {
    rt.query_selector_all("#state").await.unwrap()[0]
        .text
        .clone()
}

fn bridge(
    rt: &mut JsRuntime,
) -> (
    mpsc::Receiver<FetchRequestMsg>,
    mpsc::Sender<FetchResponseMsg>,
) {
    let (request_tx, request_rx) = mpsc::channel();
    let (response_tx, response_rx) = mpsc::channel();
    rt.set_fetch_channel(request_tx, response_rx);
    (request_rx, response_tx)
}

fn response(request: FetchRequestMsg, body: &str) -> FetchResponseMsg {
    FetchResponseMsg {
        id: request.id,
        status: 200,
        status_text: "OK".into(),
        url: request.url,
        headers: vec![],
        body: body.into(),
        body_bytes: None,
        error: None,
    }
}

#[tokio::test]
async fn timer_mutates_real_dom_without_evaluate_and_default_stays_idle() {
    let mut enabled = runtime(true).await;
    let mut disabled = runtime(false).await;
    for rt in [&mut enabled, &mut disabled] {
        eval(
            rt,
            "setTimeout(() => document.querySelector('#state').textContent = 'timer', 60)",
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(text(&mut enabled).await, "timer");
    assert_eq!(text(&mut disabled).await, "initial");
    assert!(enabled.take_dom_dirty());
}

#[tokio::test]
async fn delayed_fetch_and_animation_frame_finish_without_evaluate() {
    let mut rt = runtime(true).await;
    let (requests, responses) = bridge(&mut rt);
    eval(&mut rt, "setTimeout(() => fetch('/asset').then(r => r.text()).then(value => requestAnimationFrame(() => document.querySelector('#state').textContent = value)), 60)").await;
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request.origin.as_deref(), Some("https://main.example"));
    tokio::time::sleep(Duration::from_millis(70)).await;
    responses.send(response(request, "fetched-frame")).unwrap();
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(text(&mut rt).await, "fetched-frame");
}

#[tokio::test]
async fn replacing_document_discards_old_timers_and_late_fetch_responses() {
    let mut rt = runtime(true).await;
    let (requests, responses) = bridge(&mut rt);
    eval(&mut rt, "setTimeout(() => fetch('/old').then(r => r.text()).then(value => document.querySelector('#state').textContent = value), 40); setTimeout(() => document.querySelector('#state').textContent = 'old timer', 300)").await;
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    rt.set_document(HTML, Some("https://main.example/new"), (640, 480))
        .await
        .unwrap();
    responses.send(response(request, "old response")).unwrap();
    tokio::time::sleep(Duration::from_millis(380)).await;
    assert_eq!(text(&mut rt).await, "initial");
}

#[tokio::test]
async fn idle_main_and_child_idb_callbacks_use_their_own_origin() {
    let mut rt = runtime(true).await;
    let (requests, _responses) = bridge(&mut rt);
    rt.set_frame_document(2, HTML, "https://child.example/frame", (640, 480), vec![])
        .await
        .unwrap();
    // Each timer creates deferred IDB events after that tick's IDB drain.
    // The next main tick must not steal the child's request handlers.
    let main = rt.evaluate("setTimeout(() => { let r = indexedDB.open('main', 1); r.onsuccess = () => fetch('https://network.example/main'); }, 70)").await.unwrap();
    assert!(main.exception.is_none(), "{main:?}");
    let child = rt.evaluate_in_context("setTimeout(() => { let r = indexedDB.open('child', 1); r.onsuccess = () => fetch('https://network.example/child'); }, 70)", 2, false).await.unwrap();
    assert!(child.exception.is_none(), "{child:?}");
    let mut got = vec![];
    for _ in 0..2 {
        let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        got.push((request.url, request.origin.unwrap()));
    }
    got.sort();
    assert_eq!(
        got,
        vec![
            (
                "https://network.example/child".into(),
                "https://child.example".into()
            ),
            (
                "https://network.example/main".into(),
                "https://main.example".into()
            ),
        ]
    );
}

#[tokio::test]
async fn runaway_timer_is_limited_and_later_commands_work() {
    let mut rt = runtime(true).await;
    let (events, received) = mpsc::channel();
    rt.set_event_sink(events);
    eval(&mut rt, "setTimeout(() => { while (true) {} }, 40); setTimeout(() => document.querySelector('#state').textContent = 'recovered', 80)").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(text(&mut rt).await, "recovered");
    assert!(
        received
            .try_iter()
            .any(|event| matches!(event, oxibrowser_core::js::CoreEvent::Exception { .. }))
    );
    eval(&mut rt, "1 + 1").await;
}

#[tokio::test]
async fn idle_navigation_and_popup_effects_survive_evaluation() {
    let mut rt = runtime(true).await;
    eval(
        &mut rt,
        "setTimeout(() => { location.href = '/next'; window.open('/popup', '_blank'); }, 40)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    eval(&mut rt, "42").await;
    let mutations = rt.drain_mutations();
    assert!(
        mutations
            .iter()
            .any(|m| matches!(m, DomMutation::Navigate { url } if url.ends_with("/next"))),
        "{mutations:?}"
    );
    assert!(
        mutations
            .iter()
            .any(|m| matches!(m, DomMutation::OpenTab { url, .. } if url.ends_with("/popup"))),
        "{mutations:?}"
    );
}

#[tokio::test]
async fn autonomous_console_retention_is_bounded() {
    let mut rt = runtime(true).await;
    eval(&mut rt, "setTimeout(() => { for (let i = 0; i < 1200; i++) console.log(i); document.querySelector('#state').textContent = 'logged'; }, 40)").await;
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(text(&mut rt).await, "logged");
    assert_eq!(rt.console_output().len(), 1_000);
}

#[tokio::test]
async fn delayed_dynamic_import_survives_more_than_one_microtask_batch() {
    let mut rt = runtime(true).await;
    let (requests, responses) = bridge(&mut rt);
    eval(&mut rt, "setTimeout(() => { let n = 0; for (let i = 0; i < 300; i++) Promise.resolve().then(() => n++); import('/later.js').then(m => document.querySelector('#state').textContent = m.default + ':' + n); }, 60)").await;
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request.url, "https://main.example/later.js");
    // Import I/O has the JS timeout, not the 16 ms scheduling target.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let mut reply = response(request, "export default await Promise.resolve(9);");
    reply
        .headers
        .push(("content-type".into(), "text/javascript".into()));
    responses.send(reply).unwrap();
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert_eq!(text(&mut rt).await, "9:300");
}

#[tokio::test]
async fn interval_self_cancellation_does_not_rearm() {
    let mut rt = runtime(true).await;
    eval(&mut rt, "let hits = 0; let interval = setInterval(() => { hits++; clearInterval(interval); document.querySelector('#state').textContent = String(hits); }, 40)").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(text(&mut rt).await, "1");
}

#[tokio::test]
async fn interval_original_id_can_be_cancelled_after_multiple_ticks() {
    let mut rt = runtime(true).await;
    eval(&mut rt, "let hits = 0; let interval = setInterval(() => { hits++; document.querySelector('#state').textContent = String(hits); }, 30)").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_ne!(text(&mut rt).await, "initial");
    eval(&mut rt, "clearInterval(interval)").await;
    let stopped = text(&mut rt).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(text(&mut rt).await, stopped);
}

#[tokio::test]
async fn bootstrap_timer_errors_reach_the_event_sink_without_losing_other_timers() {
    let mut rt = runtime(false).await;
    let (events, received) = mpsc::channel();
    rt.set_event_sink(events);
    eval(&mut rt, "setTimeout(() => {throw new Error('timer callback failed')}, 0);setTimeout(() => document.querySelector('#state').textContent='survived', 0)").await;
    assert_eq!(text(&mut rt).await, "survived");
    assert!(received.try_iter().any(|event| matches!(event,
        oxibrowser_core::js::CoreEvent::Exception {message,..} if message.contains("timer callback failed"))));
}
