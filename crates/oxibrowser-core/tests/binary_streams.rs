//! Native decompression and the actual Blob -> byte stream -> Response path.

use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
use oxibrowser_core::js::runtime::JsRuntime;
use serde_json::{Value, json};
use std::io::Write;

async fn evaluate(runtime: &mut JsRuntime, expression: &str) -> Value {
    let result = runtime.evaluate_with_await(expression, true).await.unwrap();
    assert!(!result.timed_out, "{result:?}");
    assert!(result.exception.is_none(), "{result:?}");
    serde_json::from_str(result.value.as_ref().and_then(Value::as_str).unwrap()).unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn byte_literal(bytes: &[u8]) -> String {
    format!("new Uint8Array({})", serde_json::to_string(bytes).unwrap())
}

#[tokio::test]
async fn blob_decompression_response_preserves_binary_and_unicode() {
    let mut runtime = JsRuntime::new();
    let binary = [0, 128, 255, 13, 10, 65];
    let unicode = "청계천 · café 🌿";
    let compressed = byte_literal(&gzip(&binary));
    let compressed_text = byte_literal(&gzip(unicode.as_bytes()));
    let result = evaluate(
        &mut runtime,
        &format!(
            r#"
        (async () => {{
            const compressed = {compressed};
            const response = new Response(new Blob([compressed.buffer]).stream()
                .pipeThrough(new DecompressionStream('gzip')));
            const buffer = await response.arrayBuffer();
            const second = await response.arrayBuffer().then(() => 'ok', e => e.name);
            const text = await new Response(new Blob([{compressed_text}]).stream()
                .pipeThrough(new DecompressionStream('gzip'))).text();
            return JSON.stringify([buffer instanceof ArrayBuffer,
                Array.from(new Uint8Array(buffer)), text, response.bodyUsed, second]);
        }})()
    "#
        ),
    )
    .await;
    assert_eq!(result, json!([true, binary, unicode, true, "TypeError"]));
}

#[tokio::test]
async fn blob_parts_ranges_slices_and_serializer_compatibility() {
    let mut runtime = JsRuntime::new();
    let result = evaluate(&mut runtime, r#"
        (async () => {
            const storage = new Uint8Array([9, 0, 128, 255, 8]);
            const view = new DataView(storage.buffer, 1, 3);
            const blob = new Blob([view, new Uint8Array(storage.buffer, 2, 2), '한🌿'], {type: 'Text/PLAIN'});
            storage.fill(7);
            const raw = await blob.arrayBuffer();
            const copied = blob.__bytes; copied.fill(1);
            const slice = blob.slice(1, -7, 'APPLICATION/octet-stream');
            const sliced = await slice.arrayBuffer();
            const serialized = __oxi_serialize_body(slice);
            const text = await new Blob(['한🌿']).text();
            const nested = await new Blob([new Blob(['a']), 'b']).text();
            const response = new Response(new DataView(raw, 1, 2));
            return JSON.stringify({
                bytes: Array.from(new Uint8Array(await blob.arrayBuffer())),
                rawBuffer: raw instanceof ArrayBuffer,
                size: blob.size, type: blob.type, text, nested,
                slice: Array.from(new Uint8Array(sliced)), sliceType: slice.type,
                marker: slice.__isBlob, serialized: Array.from(serialized.bytes),
                response: Array.from(new Uint8Array(await response.arrayBuffer()))
            });
        })()
    "#).await;
    let mut expected = vec![0, 128, 255, 128, 255];
    expected.extend_from_slice("한🌿".as_bytes());
    assert_eq!(
        result,
        json!({
            "bytes": expected,
            "rawBuffer": true,
            "size": 12,
            "type": "text/plain",
            "text": "한🌿",
            "nested": "ab",
            "slice": [128,255,128,255],
            "sliceType": "application/octet-stream",
            "marker": true,
            "serialized": [128,255,128,255],
            "response": [128,255]
        })
    );
}

#[tokio::test]
async fn readers_enforce_locks_cancel_errors_and_pending_reads() {
    let mut runtime = JsRuntime::new();
    let result = evaluate(&mut runtime, r#"
        (async () => {
            const stream = new Blob(['abc']).stream(), reader = stream.getReader();
            let doubleLock;
            try { stream.getReader(); } catch (e) { doubleLock = e.name; }
            const cancelLocked = await stream.cancel().then(() => 'ok', e => e.name);
            reader.releaseLock();
            const released = await reader.read().then(() => 'ok', e => e.name);
            const replacement = stream.getReader();
            const first = await replacement.read(), last = await replacement.read();
            await replacement.closed; replacement.releaseLock();
            let cancelled;
            const source = new ReadableStream({cancel(reason) { cancelled = reason; }});
            const active = source.getReader(), pending = active.read();
            await active.cancel('stop');
            const cancelledRead = await pending;
            const errored = new ReadableStream({start(controller) { controller.error(new Error('source failed')); }});
            const bad = errored.getReader();
            const readError = await bad.read().then(() => 'ok', e => e.message);
            const closedError = await bad.closed.then(() => 'ok', e => e.message);
            const abandoned = new ReadableStream().getReader(), waiting = abandoned.read();
            abandoned.releaseLock();
            const pendingError = await waiting.then(() => 'ok', e => e.name);
            const releasedClosed = await replacement.closed.then(() => 'ok', e => e.name);
            let count = 0;
            const pulled = new ReadableStream({pull(controller) {
                controller.enqueue(new Uint8Array([++count]));
                if (count === 3) controller.close();
            }});
            const all = pulled.getReader();
            const reads = await Promise.all([all.read(), all.read(), all.read(), all.read()]);
            return JSON.stringify({doubleLock, cancelLocked, released,
                first: Array.from(first.value), last: last.done,
                cancelled, cancelledRead: cancelledRead.done, readError, closedError, pendingError, releasedClosed,
                pulls: reads.map(r => r.done ? null : r.value[0])});
        })()
    "#).await;
    assert_eq!(
        result,
        json!({
            "doubleLock": "TypeError", "cancelLocked": "TypeError", "released": "TypeError",
            "first": [97,98,99], "last": true, "cancelled": "stop", "cancelledRead": true,
            "readError": "source failed", "closedError": "source failed", "pendingError": "TypeError", "releasedClosed": "TypeError",
            "pulls": [1,2,3,null]
        })
    );
}

#[tokio::test]
async fn gzip_deflate_and_raw_decode_and_reject_invalid_data() {
    let mut runtime = JsRuntime::new();
    let plain = b"verified payload";
    let gzip = gzip(plain);
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::fast());
    zlib.write_all(plain).unwrap();
    let zlib = zlib.finish().unwrap();
    let mut raw = DeflateEncoder::new(Vec::new(), Compression::fast());
    raw.write_all(plain).unwrap();
    let raw = raw.finish().unwrap();
    let mut cases = Vec::new();
    for (format, bytes) in [("gzip", &gzip), ("deflate", &zlib), ("deflate-raw", &raw)] {
        cases.push(format!("[{format:?}, {}, true]", byte_literal(bytes)));
        for length in 0..bytes.len() {
            cases.push(format!(
                "[{format:?}, {}, false]",
                byte_literal(&bytes[..length])
            ));
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        cases.push(format!("[{format:?}, {}, false]", byte_literal(&trailing)));
    }
    let mut checksum = gzip.clone();
    let checksum_index = checksum.len() - 8;
    checksum[checksum_index] ^= 1;
    cases.push(format!("['gzip', {}, false]", byte_literal(&checksum)));
    let mut multiple = gzip.clone();
    multiple.extend_from_slice(&gzip);
    cases.push(format!("['gzip', {}, false]", byte_literal(&multiple)));
    let mut checksum = zlib.clone();
    let last = checksum.len() - 1;
    checksum[last] ^= 1;
    cases.push(format!("['deflate', {}, false]", byte_literal(&checksum)));
    let cases = cases.join(",");
    let result = evaluate(&mut runtime, &format!(r#"
        (async () => {{
            const failures = [];
            for (const [format, bytes, valid] of [{cases}]) {{
                try {{
                    const result = await new Response(new Blob([bytes]).stream()
                        .pipeThrough(new DecompressionStream(format))).text();
                    if (!valid || result !== 'verified payload') failures.push([format, bytes.length, 'accepted']);
                }} catch (error) {{ if (valid) failures.push([format, bytes.length, error.message]); }}
            }}
            let unsupported;
            try {{ new DecompressionStream('brotli'); }} catch (error) {{ unsupported = error.name; }}
            return JSON.stringify([failures, unsupported]);
        }})()
    "#)).await;
    assert_eq!(result, json!([[], "TypeError"]));
}

#[tokio::test]
async fn writable_decompression_propagates_errors_and_locks() {
    let mut runtime = JsRuntime::new();
    let compressed = byte_literal(&gzip(b"chunked"));
    let result = evaluate(&mut runtime, &format!(r#"
        (async () => {{
            const bytes = {compressed}, transform = new DecompressionStream('gzip');
            const writer = transform.writable.getWriter();
            let locked;
            try {{ transform.writable.getWriter(); }} catch (e) {{ locked = e.name; }}
            const output = new Response(transform.readable).text();
            await writer.write(bytes.subarray(0, 5));
            await writer.write(new DataView(bytes.buffer, 5));
            await writer.close(); await writer.closed;
            const afterClose = await writer.write(bytes).then(() => 'ok', e => e.name);
            writer.releaseLock();
            const text = await output;
            const broken = new DecompressionStream('gzip'), badWriter = broken.writable.getWriter();
            const readFailure = new Response(broken.readable).arrayBuffer().then(() => 'ok', e => e.name);
            await badWriter.write(new Uint8Array([1,2,3]));
            const closeFailure = await badWriter.close().then(() => 'ok', e => e.name);
            const closedFailure = await badWriter.closed.then(() => 'ok', e => e.name);
            const cancelled = new DecompressionStream('gzip');
            await cancelled.readable.cancel('stop');
            const cancelledWriter = cancelled.writable.getWriter();
            const cancelFailure = await cancelledWriter.write(bytes).then(() => 'ok', e => e.name);
            return JSON.stringify([locked, text, afterClose, closeFailure, await readFailure, closedFailure, cancelFailure]);
        }})()
    "#)).await;
    assert_eq!(
        result,
        json!([
            "TypeError",
            "chunked",
            "TypeError",
            "TypeError",
            "TypeError",
            "TypeError",
            "TypeError"
        ])
    );
}

#[tokio::test]
async fn decompression_rejects_decoded_output_over_128_mib() {
    let mut runtime = JsRuntime::new();
    // The compressed fixture stays small; only Rust's bounded native decoder
    // handles the 128 MiB output, with no JS per-byte array or text conversion.
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&vec![0; 128 * 1024 * 1024 + 1]).unwrap();
    let compressed = encoder.finish().unwrap();
    let literal = byte_literal(&compressed);
    let result = evaluate(&mut runtime, &format!(r#"
        new Response(new Blob([{literal}]).stream().pipeThrough(new DecompressionStream('gzip')))
            .arrayBuffer().then(() => JSON.stringify('accepted'), error => JSON.stringify([error.name, error.message]))
    "#)).await;
    assert_eq!(
        result,
        json!(["RangeError", "Buffered byte body exceeds 128 MiB limit"])
    );
}

#[tokio::test]
async fn writable_queue_limit_releases_completed_writes() {
    let mut runtime = JsRuntime::new();
    let result = evaluate(&mut runtime, r#"
        (async () => {
            const finishes=[];
            const stream = new WritableStream({write() { return new Promise(resolve => { finishes.push(resolve); }); }});
            const writer = stream.getWriter();
            const chunk = new Uint8Array(64 * 1024 * 1024);
            const first = writer.write(chunk);
            const second = writer.write(chunk);
            const oversized = await writer.write(new Uint8Array([1])).then(() => 'ok', e => e.name);
            finishes.shift()(); await first;
            const next = writer.write(chunk);
            await Promise.resolve(); await Promise.resolve();
            finishes.shift()(); await second;
            await Promise.resolve(); await Promise.resolve();
            finishes.shift()(); await next;
            await writer.close();
            return JSON.stringify(oversized);
        })()
    "#).await;
    assert_eq!(result, json!("RangeError"));
}

#[tokio::test]
async fn writable_failures_and_abort_release_pending_writes() {
    let mut runtime = JsRuntime::new();
    let result = evaluate(&mut runtime, r#"
        (async () => {
            const pending = new WritableStream({write() { return new Promise(() => {}); }}).getWriter();
            const active = pending.write(new Uint8Array([1])).then(() => 'ok', e => e);
            const queued = pending.write(new Uint8Array([2])).then(() => 'ok', e => e);
            await pending.abort('stopped');
            const errors = await Promise.all([active, queued, pending.closed.catch(e => e)]);
            const broken = new WritableStream({write() { throw new Error('write failed'); }}).getWriter();
            const failure = await broken.write(new Uint8Array([1])).then(() => 'ok', e => e.message);
            const closedFailure = await broken.closed.then(() => 'ok', e => e.message);
            return JSON.stringify([errors, failure, closedFailure]);
        })()
    "#).await;
    assert_eq!(
        result,
        json!([
            ["stopped", "stopped", "stopped"],
            "write failed",
            "write failed"
        ])
    );
}

#[tokio::test]
async fn oversized_buffer_source_is_rejected_before_native_copy() {
    let mut runtime = JsRuntime::new();
    let result = evaluate(
        &mut runtime,
        r#"
        (()=>{
            const huge=new Uint8Array(128*1024*1024+1);
            try { new Blob([huge]); } catch(error) { return JSON.stringify(error.name); }
            return JSON.stringify('accepted');
        })()
    "#,
    )
    .await;
    assert_eq!(result, json!("RangeError"));
}

#[tokio::test]
async fn response_locks_and_transform_cancellation_propagate_upstream() {
    let mut runtime = JsRuntime::new();
    let result = evaluate(&mut runtime, r#"
        (async () => {
            const response = new Response(new Blob(['body']));
            const reader = response.body.getReader();
            const locked = await response.arrayBuffer().then(() => 'ok', e => e.name);
            reader.releaseLock();
            const body = await response.text();
            const again = await response.text().then(() => 'ok', e => e.name);
            const disturbed = new Blob(['x']).stream();
            const used = disturbed.getReader(); await used.read(); used.releaseLock();
            let usedConstructor;
            try { new Response(disturbed); } catch (error) { usedConstructor = error.name; }
            let cancelled;
            const source = new ReadableStream({cancel(reason) { cancelled = reason.message; }});
            const transform = new DecompressionStream('gzip');
            const destination = source.pipeThrough(transform);
            await destination.cancel('stop');
            // Drain the pipe's cancellation and lock-release microtasks.
            for (let i = 0; i < 30; i++) await Promise.resolve();
            const errorSource = new ReadableStream({start(controller) { controller.error(new Error('upstream')); }});
            const propagated = await new Response(errorSource.pipeThrough(new DecompressionStream('gzip')))
                .arrayBuffer().then(() => 'ok', e => e.message);
            return JSON.stringify([locked, body, again, usedConstructor, cancelled, source.locked, transform.writable.locked, propagated]);
        })()
    "#).await;
    assert_eq!(
        result,
        json!([
            "TypeError",
            "body",
            "TypeError",
            "TypeError",
            "Decompression output was cancelled",
            false,
            false,
            "upstream"
        ])
    );
}
