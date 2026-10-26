//! Opt-in cooperative event loop. A tick is bounded in task count and checks
//! its soft time budget between callbacks. An engine async job uses the existing
//! cooperative executor to finish its dependencies and can exceed the ordinary
//! tick job budget. Boa VM limits bound JS execution; synchronous native host
//! calls cannot be preempted by the wall clock.

use super::*;

pub(super) const INTERVAL: Duration = Duration::from_millis(16);
const TIMER_BATCH: usize = 32;
const JOB_BATCH: usize = 128;
const EVENT_BATCH: usize = 32;

pub(super) fn activate(context_id: u32, url: &str) {
    ACTIVE_CONTEXT_ID.set(context_id);
    set_current_origin(url);
}

pub(super) fn clear_context(context_id: u32, queue: &Rc<TokioJobQueue>) {
    queue.clear();
    PENDING_FETCH.with(|pending| {
        pending
            .borrow_mut()
            .retain(|_, p| p.context_id() != context_id)
    });
    DEFERRED_RESPONSES.with(|deferred| {
        deferred
            .borrow_mut()
            .retain(|response| PENDING_FETCH.with(|p| p.borrow().contains_key(&response.id)));
    });
    let sockets: Vec<u64> = PENDING_WS.with(|pending| {
        pending
            .borrow()
            .iter()
            .filter_map(|(id, state)| (state.context_id() == Some(context_id)).then_some(*id))
            .collect()
    });
    for id in sockets {
        WS_REQ_TX.with(|tx| {
            if let Some(tx) = tx.borrow().as_ref() {
                let _ = tx.send(WsReqMsg::Close {
                    id,
                    code: None,
                    reason: None,
                });
            }
        });
        PENDING_WS.with(|pending| {
            pending.borrow_mut().remove(&id);
        });
    }
    PENDING_WS.with(|pending| {
        pending
            .borrow_mut()
            .retain(|_, state| !matches!(state, WsState::Closed))
    });
    DEFERRED_WS_EVENTS.with(|events| {
        events.borrow_mut().retain(|event| {
            let id = match event {
                WsEvent::Open { id, .. }
                | WsEvent::Message { id, .. }
                | WsEvent::Close { id, .. }
                | WsEvent::Error { id, .. } => id,
            };
            PENDING_WS.with(|pending| pending.borrow().contains_key(id))
        })
    });
    super::super::idb::clear_context(context_id);
    LISTENER_REGISTRY.with(|r| r.borrow_mut().retain(|(owner, _), _| *owner != context_id));
    NATIVE_LISTENERS.with(|r| r.borrow_mut().retain(|(owner, _), _| *owner != context_id));
}

pub(super) fn tick(ctx: &mut Context, queue: &Rc<TokioJobQueue>, config: &JsRuntimeConfig) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let limits = ctx.runtime_limits_mut();
        limits.set_loop_iteration_limit(config.nav_script_max_loop_iterations);
        limits.set_recursion_limit(config.nav_script_max_recursion);
        limits.set_stack_size_limit(config.nav_script_max_stack_size);
        // Module loading is synchronous and caches failures. Its I/O deadline
        // is the configured JS timeout, independently of our 16 ms tick target.
        let _module_deadline = ctx
            .downcast_module_loader::<modules::WebModuleLoader>()
            .map(|loader| loader.limit_for(Duration::from_millis(config.timeout_ms.max(1))));
        let start = Instant::now();
        drain_pending_fetch_responses_bounded(ctx, EVENT_BATCH, false);
        drain_ws_events_bounded(ctx, EVENT_BATCH);
        super::super::idb::drain_idb_events_bounded(ctx, EVENT_BATCH);
        let _ = queue.run_background_jobs(ctx, JOB_BATCH);
        // Pop one timer at a time so callbacks that cancel other timers can
        // prevent them from firing, and no task is lost at the time boundary.
        for _ in 0..TIMER_BATCH {
            if start.elapsed() >= INTERVAL {
                break;
            }
            let Some(timer) = queue.pop_due_timers_bounded(1).pop() else {
                break;
            };
            let result = timer.callback.call(&JsValue::undefined(), &timer.args, ctx);
            queue.complete_timer(timer, result.is_ok());
            if let Err(error) = result {
                let (message, name, stack) = error_sink_details(&error, ctx);
                push_event(CoreEvent::Exception {
                    message,
                    name,
                    stack,
                    timestamp: now_ms(),
                });
            }
            let _ = queue.run_background_jobs(ctx, JOB_BATCH);
        }
    }));
    if let Err(payload) = outcome {
        // A poisoned native callback must not kill the command thread or be
        // retried forever; discard that context's queued work and keep its DOM.
        clear_context(active_context_id(), queue);
        tracing::warn!(detail = %panic_message(payload.as_ref()), "background JS tick panicked");
    }
}
