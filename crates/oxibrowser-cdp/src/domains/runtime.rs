//! CDP Runtime domain handler.
//!
//! Handles Runtime.enable, Runtime.disable, Runtime.evaluate,
//! Runtime.callFunctionOn, Runtime.runScript, Runtime.getProperties.
//!
//! After Runtime.enable, emits Runtime.executionContextCreated.
//! Runtime.evaluate delegates to boa_engine via Session::evaluate_js().

use crate::domains::{DispatchContext, DomainResult};
use crate::event::EventSender;
use crate::protocol::CdpError;
use serde_json::{Value, json};

/// Dispatch Runtime domain methods.
pub async fn handle(method: &str, params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    match method {
        "enable" => enable(ctx).await,
        "addBinding" => remote("addBinding", params, ctx).await,
        // This engine does not suspend execution at a debugger barrier.
        "runIfWaitingForDebugger" => Ok(Some(json!({}))),
        "disable" => disable(ctx),
        "evaluate" => remote("evaluate", params, ctx).await,
        "callFunctionOn" => remote("callFunctionOn", params, ctx).await,
        "getProperties" => remote("getProperties", params, ctx).await,
        "releaseObject" => remote("releaseObject", params, ctx).await,
        "releaseObjectGroup" => remote("releaseObjectGroup", params, ctx).await,
        "runScript" => run_script(params, ctx).await,
        _ => Err(CdpError {
            code: -32601,
            message: format!("Runtime.{} not implemented", method),
        }),
    }
}

/// Runtime.enable — enables runtime event reporting.
async fn enable(ctx: &DispatchContext) -> DomainResult {
    let guard = ctx.session.read().await;
    ctx.events.clear_reported_worlds(&guard.id().to_string());
    ctx.events.set_runtime_enabled(true);

    // Main frame execution context (always id=1).
    ctx.events.send_runtime_event(
        "Runtime.executionContextCreated",
        json!({
            "context": {
                "id": 1,
                "origin": "",
                "name": "main",
                "uniqueId": format!("context-{}", uuid::Uuid::new_v4()),
                "auxData": {
                    "isDefault": true,
                    "type": "default",
                    "frameId": ctx.events.main_frame_id()
                }
            }
        }),
    );

    // Child frame execution contexts (Phase 8).
    let frame_map = guard.frame_context_map().read().clone();
    for (frame_id, context_id) in &frame_map {
        ctx.events.send_runtime_event(
            "Runtime.executionContextCreated",
            json!({
                "context": {
                    "id": context_id,
                    "origin": "",
                    "name": format!("iframe:{frame_id}"),
                    "uniqueId": format!("context-{}", uuid::Uuid::new_v4()),
                    "auxData": {
                        "isDefault": true,
                        "type": "default",
                        "frameId": frame_id
                    }
                }
            }),
        );
    }
    for ((frame, world), context_id) in guard.isolated_worlds() {
        if !ctx
            .events
            .report_world_once(&guard.id().to_string(), *context_id)
        {
            continue;
        }
        ctx.events.send_runtime_event("Runtime.executionContextCreated", json!({"context":{
            "id":context_id, "origin":"", "name":world, "uniqueId":format!("context-{}",uuid::Uuid::new_v4()),
            "auxData":{"isDefault":false,"type":"isolated","frameId":if frame == "main" {ctx.events.main_frame_id()} else {frame.as_str()}}
        }}));
    }
    drop(guard);

    Ok(Some(json!({})))
}

/// Runtime.disable — disables runtime event reporting.
fn disable(ctx: &DispatchContext) -> DomainResult {
    ctx.events.set_runtime_enabled(false);
    Ok(Some(json!({})))
}

/// Forward native remote-object commands to the owning execution context.
pub(super) async fn remote(
    method: &str,
    params: Option<Value>,
    ctx: &DispatchContext,
) -> DomainResult {
    let params = params.unwrap_or_else(|| json!({}));
    if method == "addBinding" && params.get("executionContextName").is_some() {
        return Err(CdpError {
            code: -32000,
            message: "Binding scopes by executionContextName are not supported".into(),
        });
    }
    let explicit_context = params
        .get("contextId")
        .or_else(|| params.get("executionContextId"))
        .and_then(Value::as_u64);
    let object_context = params
        .get("objectId")
        .and_then(Value::as_str)
        .and_then(|id| id.strip_prefix("oxi-remote-"))
        .and_then(|id| id.split('-').next())
        .and_then(|id| id.parse::<u32>().ok());
    let context_id = match explicit_context {
        Some(id) => u32::try_from(id).map_err(|_| CdpError {
            code: -32602,
            message: "Invalid execution context id".into(),
        })?,
        None => object_context.unwrap_or(1),
    };
    let mut guard = ctx.session.write().await;
    let mut result = guard
        .remote_command(context_id, json!({"method":method, "params":params}))
        .await
        .map_err(|error| CdpError {
            code: -32000,
            message: error.to_string(),
        })?;
    if let Some(output) = result
        .as_object_mut()
        .and_then(|result| result.remove("_consoleOutput"))
        && let Some(messages) = output.as_array()
        && !messages.is_empty()
    {
        let args: Vec<Value> = messages
            .iter()
            .map(|message| json!({"type":"string","value":message,"description":message}))
            .collect();
        ctx.events.send_runtime_event("Runtime.consoleAPICalled",json!({"type":"log","args":args,"executionContextId":context_id,"timestamp":EventSender::timestamp_ms()}));
    }
    Ok(Some(result))
}

/// No compiled script store exists yet; explicit source follows evaluate.
async fn run_script(params: Option<Value>, ctx: &DispatchContext) -> DomainResult {
    let mut params = params.unwrap_or_else(|| json!({}));
    let script = params
        .get("script")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError {
            code: -32602,
            message:
                "Runtime.runScript requires script source; compiled script IDs are unsupported"
                    .into(),
        })?
        .to_owned();
    params["expression"] = json!(script);
    remote("evaluate", Some(params), ctx).await
}
