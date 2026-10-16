//! Bridge from core-originated [`CoreEvent`]s to CDP events.
//!
//! The JS thread (in `oxibrowser-core`) cannot name CDP types, so it emits a
//! neutral [`CoreEvent`] enum onto a shared channel. The CDP session spawns a
//! drainer task that calls [`emit_core_event`] for each one, translating it
//! into the appropriate CDP events (gated by the relevant domain's enabled
//! flag on the [`EventSender`]).

use crate::event::EventSender;
use oxibrowser_core::js::{ConsoleArg, ConsoleLevel, CoreEvent, WsDirection};
use serde_json::{Value, json};

/// Translate one [`CoreEvent`] into its CDP event(s) on the given sender.
///
/// Each translation is independently gated by the owning domain's enabled flag,
/// mirroring real Chrome: a `console.log` emits both `Runtime.consoleAPICalled`
/// (gated by `Runtime.enable`) and `Log.entryAdded` (gated by `Log.enable`).
pub fn emit_core_event(events: &EventSender, ev: CoreEvent) {
    emit_core_event_opt(events, ev, None);
}

/// Translate a CoreEvent originating from a child target's session, stamping
/// the emitted CDP events with `session_id` instead of the attached session.
pub fn emit_core_event_with_session(events: &EventSender, ev: CoreEvent, session_id: &str) {
    emit_core_event_opt(events, ev, Some(session_id));
}

fn emit_core_event_opt(events: &EventSender, ev: CoreEvent, session: Option<&str>) {
    match ev {
        CoreEvent::Console {
            level,
            args,
            timestamp,
        } => emit_console(events, level, args, timestamp, session),
        CoreEvent::Exception {
            message,
            name,
            stack,
            timestamp,
        } => emit_exception(events, message, name, stack, timestamp, session),
        CoreEvent::FetchRequest {
            request_id,
            url,
            method,
            headers,
            post_data,
            timestamp,
        } => emit_request_will_be_sent(
            events, request_id, url, method, headers, post_data, "XHR", timestamp, session,
        ),
        CoreEvent::FetchResponse {
            request_id,
            url,
            status,
            mime_type,
            timestamp,
        } => emit_response_received(
            events, request_id, url, status, mime_type, "XHR", timestamp, session,
        ),
        CoreEvent::SubresourceFetchRequest {
            request_id,
            url,
            method,
            resource_type,
            timestamp,
        } => emit_request_will_be_sent(
            events,
            request_id,
            url,
            method,
            Vec::new(),
            None,
            &resource_type,
            timestamp,
            session,
        ),
        CoreEvent::SubresourceFetchResponse {
            request_id,
            url,
            status,
            mime_type,
            length,
            timestamp,
        } => {
            emit_response_received(
                events,
                request_id.clone(),
                url.clone(),
                status,
                mime_type,
                // The paired requestWillBeSent already carried the resource
                // type; response type mirrors it via the same event id.
                "Other",
                timestamp,
                session,
            );
            send_net(
                events,
                session,
                "Network.loadingFinished",
                json!({
                    "requestId": request_id,
                    "timestamp": timestamp / 1000.0,
                    "encodedDataLength": length as f64,
                }),
            );
        }
        CoreEvent::SubresourceFetchFailed {
            request_id,
            url,
            error_text,
            timestamp,
        } => {
            emit_request_will_be_sent(
                events,
                request_id.clone(),
                url,
                "GET".to_string(),
                Vec::new(),
                None,
                "Other",
                timestamp,
                session,
            );
            send_net(
                events,
                session,
                "Network.loadingFailed",
                json!({
                    "requestId": request_id,
                    "timestamp": timestamp / 1000.0,
                    "errorText": error_text,
                    "canceled": false,
                }),
            );
        }
        CoreEvent::FetchLoadingFinished {
            request_id,
            timestamp,
        } => {
            send_net(
                events,
                session,
                "Network.loadingFinished",
                json!({
                    "requestId": request_id,
                    "timestamp": timestamp / 1000.0,
                    "encodedDataLength": 0.0,
                }),
            );
        }
        CoreEvent::WsFrame {
            direction,
            request_id,
            opcode: _,
            data,
            timestamp,
        } => {
            let method = match direction {
                WsDirection::Sent => "Network.webSocketFrameSent",
                WsDirection::Received => "Network.webSocketFrameReceived",
            };
            send_net(
                events,
                session,
                method,
                json!({
                    "requestId": request_id,
                    "timestamp": timestamp / 1000.0,
                    "response": {
                        "opcode": 1,
                        "mask": true,
                        "payloadData": data,
                    },
                }),
            );
        }
        CoreEvent::Dialog {
            dialog_type,
            message,
            default_value,
        } => {
            send_page(
                events,
                session,
                "Page.javascriptDialogOpening",
                json!({
                    "url": "",
                    "message": message,
                    "type": dialog_type.as_str(),
                    "defaultPrompt": default_value.unwrap_or_default(),
                }),
            );
        }
        CoreEvent::Download {
            guid,
            url,
            filename,
            save_path,
            total_bytes,
        } => {
            send_page(
                events,
                session,
                "Page.downloadWillBegin",
                json!({
                    "frameId": "",
                    "guid": guid,
                    "url": url,
                    "suggestedFilename": filename,
                }),
            );
            send_page(
                events,
                session,
                "Page.downloadProgress",
                json!({
                    "guid": guid,
                    "totalBytes": total_bytes,
                    "receivedBytes": total_bytes,
                    "state": "completed",
                    "filePath": save_path,
                }),
            );
            // Browser.* twins with identical payloads. Sent ungated via
            // `send_browser_event` (no Browser.enable flag exists); always on
            // the root session — downloads from child (attached) targets are
            // not sessionId-stamped (known limitation, see send_browser_event).
            events.send_browser_event(
                "Browser.downloadWillBegin",
                json!({
                    "frameId": "",
                    "guid": guid,
                    "url": url,
                    "suggestedFilename": filename,
                }),
            );
            events.send_browser_event(
                "Browser.downloadProgress",
                json!({
                    "guid": guid,
                    "totalBytes": total_bytes,
                    "receivedBytes": total_bytes,
                    "state": "completed",
                    "filePath": save_path,
                }),
            );
        }
        CoreEvent::TabSpawned {
            url,
            target,
            source_session_id,
        } => {
            // Page-initiated new-tab request (window.open / target=_blank).
            // The CDP layer does not mint the session here — hosts that
            // want popup support create it (the agent-plane EventHub does;
            // see CoreEvent::TabSpawned docs). The event surfaces as a
            // custom OXI notification for observability.
            events.send_browser_event(
                "OXI.tabSpawned",
                json!({
                    "url": url,
                    "target": target,
                    "sourceSessionId": source_session_id,
                }),
            );
        }
        CoreEvent::DownloadFailed {
            guid,
            url: _,
            error: _,
        } => {
            // Playwright/Puppeteer watch for `state != "inprogress"`; "failed"
            // with no filePath signals the save itself failed.
            send_page(
                events,
                session,
                "Page.downloadProgress",
                json!({
                    "guid": guid,
                    "totalBytes": 0,
                    "receivedBytes": 0,
                    "state": "failed",
                    "filePath": "",
                }),
            );
            // Browser.* twin, same payload, ungated (see Download arm).
            events.send_browser_event(
                "Browser.downloadProgress",
                json!({
                    "guid": guid,
                    "totalBytes": 0,
                    "receivedBytes": 0,
                    "state": "failed",
                    "filePath": "",
                }),
            );
        }
        CoreEvent::RequestPaused {
            request_id,
            url,
            method,
            headers,
            resource_type,
            timestamp: _,
        } => {
            let headers_json: serde_json::Map<String, serde_json::Value> =
                headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            let params = json!({
                "requestId": request_id,
                "request": {
                    "url": url,
                    "method": method,
                    "headers": headers_json,
                    "initialPriority": "VeryHigh",
                    "urlFragment": "",
                    "postData": serde_json::Value::Null,
                },
                "resourceType": resource_type,
                "frameId": "main",
                "networkIntercepted": true,
            });
            match session {
                Some(sid) => {
                    events.send_fetch_event_with_session("Fetch.requestPaused", params, sid)
                }
                None => events.send_fetch_event("Fetch.requestPaused", params),
            }
        }
    }
}

/// Send a Network event, routing to the with-session variant when a session is
/// supplied (child-target events).
fn send_net(events: &EventSender, session: Option<&str>, method: &str, params: Value) {
    match session {
        Some(sid) => events.send_network_event_with_session(method, params, sid),
        None => events.send_network_event(method, params),
    }
}

/// Send a Page event, routing to the with-session variant when a session is
/// supplied (child-target events).
fn send_page(events: &EventSender, session: Option<&str>, method: &str, params: Value) {
    match session {
        Some(sid) => events.send_page_event_with_session(method, params, sid),
        None => events.send_page_event(method, params),
    }
}

fn emit_console(
    events: &EventSender,
    level: ConsoleLevel,
    args: Vec<ConsoleArg>,
    timestamp: f64,
    session: Option<&str>,
) {
    // Runtime.consoleAPICalled — typed RemoteObjects (number/boolean/object/
    // null/undefined), not always-string.
    let remote_args: Vec<Value> = args.iter().map(console_arg_to_remote_object).collect();
    let api_params = json!({
        "type": level.api_type(),
        "args": remote_args,
        "executionContextId": 1,
        "timestamp": timestamp,
    });
    match session {
        Some(sid) => {
            events.send_runtime_event_with_session("Runtime.consoleAPICalled", api_params, sid)
        }
        None => events.send_runtime_event("Runtime.consoleAPICalled", api_params),
    }

    // Log.entryAdded — mirror console messages into the Log domain.
    let text = args
        .iter()
        .map(|a| a.display())
        .collect::<Vec<_>>()
        .join(" ");
    let log_params = json!({
        "entry": {
            "source": "console",
            "level": log_level(&level),
            "text": text,
            "timestamp": timestamp,
            "url": Value::Null,
            "lineNumber": Value::Null,
        }
    });
    match session {
        Some(sid) => events.send_log_event_with_session("Log.entryAdded", log_params, sid),
        None => events.send_log_event("Log.entryAdded", log_params),
    }
}

/// Map a [`ConsoleArg`] to a CDP `RemoteObject` JSON value.
fn console_arg_to_remote_object(arg: &ConsoleArg) -> Value {
    match arg {
        ConsoleArg::String(s) => json!({ "type": "string", "value": s }),
        ConsoleArg::Number(n) => {
            // CDP encodes NaN/Infinity as their string forms inside `value`.
            let value = if n.is_nan() {
                json!("NaN")
            } else if *n == f64::INFINITY {
                json!("Infinity")
            } else if *n == f64::NEG_INFINITY {
                json!("-Infinity")
            } else {
                json!(n)
            };
            json!({ "type": "number", "value": value })
        }
        ConsoleArg::Boolean(b) => json!({ "type": "boolean", "value": b }),
        ConsoleArg::Null => json!({ "type": "object", "subtype": "null", "value": Value::Null }),
        ConsoleArg::Undefined => json!({ "type": "undefined" }),
        ConsoleArg::Object {
            class_name,
            description,
        } => json!({
            "type": "object",
            "className": class_name,
            "description": description,
        }),
    }
}

fn emit_exception(
    events: &EventSender,
    message: String,
    name: String,
    stack: Option<String>,
    timestamp: f64,
    session: Option<&str>,
) {
    // boa 0.20 has no real source locations; the stack (if any) is a synthetic
    // trace string. Surface it as the stackTrace description with a single
    // placeholder frame so clients that demand a non-empty callFrames array
    // don't choke.
    let stack_trace = match &stack {
        Some(s) => json!({
            "callFrames": [{ "functionName": "", "scriptId": "0", "url": "", "lineNumber": 0, "columnNumber": 0 }],
            "description": s
        }),
        None => json!({ "callFrames": [] }),
    };
    let params = json!({
        "timestamp": timestamp,
        "exceptionDetails": {
            "exceptionId": 1,
            "text": message,
            "lineNumber": 0,
            "columnNumber": 0,
            "scriptId": "0",
            "stackTrace": stack_trace,
            "exception": {
                "type": "object",
                "subtype": "error",
                "className": name,
                "description": message,
            }
        }
    });
    match session {
        Some(sid) => events.send_runtime_event_with_session("Runtime.exceptionThrown", params, sid),
        None => events.send_runtime_event("Runtime.exceptionThrown", params),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_request_will_be_sent(
    events: &EventSender,
    request_id: String,
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    post_data: Option<Vec<u8>>,
    resource_type: &str,
    timestamp: f64,
    session: Option<&str>,
) {
    let hdrs: Value = headers
        .into_iter()
        .fold(serde_json::Map::new(), |mut acc, (k, v)| {
            acc.insert(k, Value::String(v));
            acc
        })
        .into();
    let mut request = json!({
        "url": url,
        "method": method,
        "headers": hdrs,
        "initialPriority": "High",
        "urlFragment": "",
    });
    if let Some(body) = post_data
        && let Ok(s) = String::from_utf8(body)
    {
        request["postData"] = Value::String(s);
    }
    send_net(
        events,
        session,
        "Network.requestWillBeSent",
        json!({
            "requestId": request_id,
            "loaderId": "0",
            "documentURL": "",
            "request": request,
            "timestamp": timestamp / 1000.0,
            "wallTime": timestamp / 1000.0,
            "initiator": { "type": "script" },
            "type": resource_type,
            "frameId": "main",
            "hasUserGesture": false,
        }),
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_response_received(
    events: &EventSender,
    request_id: String,
    url: String,
    status: u16,
    mime_type: String,
    resource_type: &str,
    timestamp: f64,
    session: Option<&str>,
) {
    let (hdrs, mime) = if mime_type.is_empty() {
        (json!({}), "text/plain")
    } else {
        (json!({ "Content-Type": mime_type }), mime_type.as_str())
    };
    send_net(
        events,
        session,
        "Network.responseReceived",
        json!({
            "requestId": request_id,
            "loaderId": "0",
            "timestamp": timestamp / 1000.0,
            "type": resource_type,
            "response": {
                "url": url,
                "status": status,
                "statusText": "",
                "headers": hdrs,
                "mimeType": mime,
                "connectionReused": false,
                "connectionId": 0.0,
                "encodedDataLength": 0.0,
                "securityState": "secure",
            },
            "frameId": "main",
        }),
    );
}

/// Map a [`ConsoleLevel`] to a `Log.entryAdded` level string.
fn log_level(level: &ConsoleLevel) -> &'static str {
    match level {
        ConsoleLevel::Log | ConsoleLevel::Info => "info",
        ConsoleLevel::Warn => "warning",
        ConsoleLevel::Error => "error",
    }
}
