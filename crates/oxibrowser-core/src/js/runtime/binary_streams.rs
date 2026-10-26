//! Bounded, buffered byte streams for Blob/Response and native decompression.
//!
//! This implements byte-stream consumption and reader/writer lifecycle, not the
//! complete Streams standard: no BYOB, tee, incremental decompression or
//! backpressure strategy. Decompression validates the entire input before
//! publishing output, and all buffered bodies are limited to 128 MiB.

use super::*;
use boa_engine::object::builtins::{AlignedVec, JsArrayBuffer, JsDataView, JsTypedArray};
use flate2::{Crc, Decompress, FlushDecompress, Status};

const MAX_BYTES: usize = 128 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> boa_engine::JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

fn checked_size(size: usize) -> JsResult<()> {
    if size > MAX_BYTES {
        return Err(JsNativeError::range()
            .with_message("Buffered byte body exceeds 128 MiB limit")
            .into());
    }
    Ok(())
}

fn bytes(value: &JsValue, ctx: &mut Context) -> JsResult<Vec<u8>> {
    let bytes = if let Some(value) = value.as_string() {
        checked_size(value.len())?;
        value.to_std_string_lossy().into_bytes()
    } else {
        if let Some(object) = value.as_object() {
            let length = if let Ok(buffer) = JsArrayBuffer::from_object(object.clone()) {
                buffer
                    .data()
                    .ok_or_else(|| invalid("ArrayBuffer is detached"))?
                    .len()
            } else if let Ok(view) = JsTypedArray::from_object(object.clone()) {
                view.byte_length(ctx)?
            } else if let Ok(view) = JsDataView::from_object(object) {
                view.byte_length(ctx)? as usize
            } else {
                return Err(invalid("Expected an ArrayBuffer or view"));
            };
            checked_size(length)?;
        }
        copy_buffer_source(value, ctx)?
    };
    checked_size(bytes.len())?;
    Ok(bytes)
}

fn buffer(bytes: Vec<u8>, ctx: &mut Context) -> JsResult<JsValue> {
    JsArrayBuffer::from_byte_block(AlignedVec::from_iter(0, bytes), ctx).map(Into::into)
}

/// Require an actual end marker, rather than accepting an exhausted but
/// incomplete input. For zlib, StreamEnd also verifies the Adler-32 checksum.
fn inflate(input: &[u8], zlib: bool) -> JsResult<(Vec<u8>, usize)> {
    let mut decoder = Decompress::new(zlib);
    let mut output = Vec::new();
    let mut chunk = [0u8; 32 * 1024];
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let capacity = chunk.len().min(MAX_BYTES - output.len() + 1);
        let status = decoder
            .decompress(
                &input[before_in as usize..],
                &mut chunk[..capacity],
                // Finish requires enough output space for some backends.
                // None allows bounded chunks; StreamEnd is still mandatory.
                FlushDecompress::None,
            )
            .map_err(|error| invalid(format!("Invalid compressed data: {error}")))?;
        let written = (decoder.total_out() - before_out) as usize;
        checked_size(output.len() + written)?;
        output.extend_from_slice(&chunk[..written]);
        if status == Status::StreamEnd {
            return Ok((output, decoder.total_in() as usize));
        }
        if decoder.total_in() == before_in && decoder.total_out() == before_out {
            return Err(invalid("Truncated compressed data"));
        }
    }
}

fn decompress(input: &[u8], format: &str) -> JsResult<Vec<u8>> {
    checked_size(input.len())?;
    match format {
        "deflate" | "deflate-raw" => {
            let (output, consumed) = inflate(input, format == "deflate")?;
            if consumed != input.len() {
                return Err(invalid("Trailing compressed data"));
            }
            Ok(output)
        }
        "gzip" => {
            // Parse the gzip envelope separately so the raw inflater must
            // reach StreamEnd before the checksum/size trailer is examined.
            if input.len() < 18 || input[..3] != [0x1f, 0x8b, 8] || input[3] & 0xe0 != 0 {
                return Err(invalid("Invalid gzip header"));
            }
            let flags = input[3];
            let mut start = 10usize;
            if flags & 4 != 0 {
                let length = input
                    .get(start..start + 2)
                    .ok_or_else(|| invalid("Truncated gzip header"))?;
                start += 2 + u16::from_le_bytes([length[0], length[1]]) as usize;
                if start > input.len() {
                    return Err(invalid("Truncated gzip header"));
                }
            }
            for flag in [8, 16] {
                if flags & flag != 0 {
                    start += input[start..]
                        .iter()
                        .position(|byte| *byte == 0)
                        .ok_or_else(|| invalid("Truncated gzip header"))?
                        + 1;
                }
            }
            if flags & 2 != 0 {
                let stored = input
                    .get(start..start + 2)
                    .ok_or_else(|| invalid("Truncated gzip header checksum"))?;
                let mut crc = Crc::new();
                crc.update(&input[..start]);
                if crc.sum() as u16 != u16::from_le_bytes([stored[0], stored[1]]) {
                    return Err(invalid("Invalid gzip header checksum"));
                }
                start += 2;
            }
            let (output, consumed) = inflate(&input[start..], false)?;
            let trailer = input
                .get(start + consumed..)
                .filter(|trailer| trailer.len() == 8)
                .ok_or_else(|| invalid("Truncated or trailing gzip data"))?;
            let expected_crc = u32::from_le_bytes(trailer[..4].try_into().unwrap());
            let expected_size = u32::from_le_bytes(trailer[4..].try_into().unwrap());
            let mut crc = Crc::new();
            crc.update(&output);
            if crc.sum() != expected_crc || output.len() as u32 != expected_size {
                return Err(invalid("Invalid gzip checksum or size"));
            }
            Ok(output)
        }
        _ => Err(invalid("Unsupported compression format")),
    }
}

pub(super) fn register(ctx: &mut Context) -> JsResult<()> {
    ctx.register_global_callable(
        js_string!("__oxi_byte_copy"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            buffer(
                bytes(args.first().unwrap_or(&JsValue::undefined()), ctx)?,
                ctx,
            )
        }),
    )?;
    ctx.register_global_callable(
        js_string!("__oxi_byte_concat"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let parts = args
                .first()
                .and_then(JsValue::as_object)
                .ok_or_else(|| invalid("Expected byte parts"))?;
            let length = parts.get(js_string!("length"), ctx)?.to_length(ctx)?;
            let mut output = Vec::new();
            for index in 0..length {
                let part = parts.get(index, ctx)?;
                let part = bytes(&part, ctx)?;
                checked_size(output.len().saturating_add(part.len()))?;
                output.extend_from_slice(&part);
            }
            buffer(output, ctx)
        }),
    )?;
    ctx.register_global_callable(
        js_string!("__oxi_byte_text"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let bytes = bytes(args.first().unwrap_or(&JsValue::undefined()), ctx)?;
            let text = String::from_utf8_lossy(&bytes);
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            Ok(JsString::from(text).into())
        }),
    )?;
    ctx.register_global_callable(
        js_string!("__oxi_byte_decompress"),
        2,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let input = bytes(args.first().unwrap_or(&JsValue::undefined()), ctx)?;
            let format = args
                .get(1)
                .unwrap_or(&JsValue::undefined())
                .to_string(ctx)?
                .to_std_string_lossy();
            buffer(decompress(&input, &format)?, ctx)
        }),
    )?;
    ctx.eval(Source::from_bytes(BOOTSTRAP))?;
    Ok(())
}

const BOOTSTRAP: &str = r#"
(function () {
  const copy = globalThis.__oxi_byte_copy, concat = globalThis.__oxi_byte_concat;
  const text = globalThis.__oxi_byte_text, decompress = globalThis.__oxi_byte_decompress;
  delete globalThis.__oxi_byte_copy; delete globalThis.__oxi_byte_concat;
  delete globalThis.__oxi_byte_text; delete globalThis.__oxi_byte_decompress;
  const names = ['Blob', 'Response', 'ReadableStream', 'ReadableStreamDefaultReader',
                 'WritableStream', 'WritableStreamDefaultWriter', 'DecompressionStream'];
  if (globalThis.ReadableStream && globalThis.ReadableStream.__oxiBufferedBytes) {
    if (globalThis.window) for (const name of names) window[name] = globalThis[name];
    return;
  }
  const LIMIT = 128 * 1024 * 1024;
  const streams = new WeakMap(), readers = new WeakMap(), writers = new WeakMap();
  const sinks = new WeakMap(), blobs = new WeakMap(), responses = new WeakMap();
  const fail = message => new TypeError(message);
  function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    promise.catch(() => {});
    return { promise, resolve, reject };
  }
  function required(map, object) {
    const state = map.get(object);
    if (!state) throw fail('Incompatible receiver');
    return state;
  }
  function byteChunk(value) {
    if (!(value instanceof ArrayBuffer) && !ArrayBuffer.isView(value))
      throw fail('Byte streams require an ArrayBuffer or view');
    return new Uint8Array(copy(value));
  }
  function settleClosed(state) {
    if (state.closing && !state.queue.length && state.status === 'readable') {
      state.status = 'closed';
      if (state.reader) readers.get(state.reader).closed.resolve();
      while (state.pending.length) state.pending.shift().resolve({ value: undefined, done: true });
    }
  }
  function errorStream(state, error) {
    if (state.status !== 'readable') return;
    state.status = 'errored'; state.error = error; state.queue = []; state.size = 0;
    if (state.reader) readers.get(state.reader).closed.reject(error);
    while (state.pending.length) state.pending.shift().reject(error);
  }
  function deliver(state) {
    while (state.pending.length && state.queue.length) {
      const value = state.queue.shift(); state.size -= value.byteLength;
      state.pending.shift().resolve({ value, done: false });
    }
    settleClosed(state);
  }
  function pull(state) {
    if (!state.started || state.pulling || state.status !== 'readable' || state.closing || !state.pending.length) return;
    if (typeof state.source.pull !== 'function') return;
    state.pulling = true;
    const version = state.version;
    Promise.resolve().then(() => state.source.pull(state.controller)).then(() => {
      state.pulling = false;
      deliver(state);
      if (state.version !== version) pull(state);
    }, error => { state.pulling = false; errorStream(state, error); });
  }
  async function cancelStream(state, reason) {
    state.disturbed = true;
    if (state.status === 'errored') throw state.error;
    if (state.status === 'closed') return;
    state.queue = []; state.size = 0; state.closing = true; settleClosed(state);
    if (typeof state.source.cancel === 'function') await state.source.cancel(reason);
  }
  class ReadableStream {
    constructor(source = {}, strategy = {}) {
      if (source.type !== undefined || strategy.size !== undefined || strategy.highWaterMark !== undefined)
        throw fail('Byte stream strategies and BYOB are unsupported');
      const state = { source, status: 'readable', queue: [], size: 0, closing: false,
        reader: null, pending: [], started: false, pulling: false, disturbed: false, version: 0 };
      streams.set(this, state);
      const controller = {
        get desiredSize() { return state.status === 'errored' ? null : state.closing ? 0 : 1 - state.queue.length; },
        enqueue(value) {
          if (state.status !== 'readable' || state.closing) throw fail('Stream is not readable');
          const chunk = byteChunk(value);
          if (state.size + chunk.byteLength > LIMIT) {
            const error = new RangeError('Buffered byte body exceeds 128 MiB limit');
            errorStream(state, error); throw error;
          }
          state.queue.push(chunk); state.size += chunk.byteLength; state.version++; deliver(state); pull(state);
        },
        close() {
          if (state.status !== 'readable' || state.closing) throw fail('Stream is not readable');
          state.closing = true; settleClosed(state);
        },
        error(error) { errorStream(state, error); }
      };
      state.controller = controller;
      const start = typeof source.start === 'function' ? source.start(controller) : undefined;
      Promise.resolve(start).then(() => { state.started = true; pull(state); }, error => errorStream(state, error));
    }
    get locked() { return !!required(streams, this).reader; }
    getReader(options = {}) {
      if (options.mode !== undefined) throw fail('BYOB readers are unsupported');
      return new ReadableStreamDefaultReader(this);
    }
    cancel(reason) {
      const state = required(streams, this);
      return state.reader ? Promise.reject(fail('Stream is locked')) : cancelStream(state, reason);
    }
    pipeTo(destination, options = {}) {
      let reader, writer;
      try {
        if (Object.keys(options).length) throw fail('Pipe options are unsupported');
        if (!(destination instanceof WritableStream)) throw fail('Expected WritableStream');
        if (this.locked || destination.locked) throw fail('Stream is locked');
        reader = this.getReader(); writer = destination.getWriter();
      } catch (error) { return Promise.reject(error); }
      // Destination errors must cancel an idle upstream read too, including
      // cancellation of a transform's readable before more input arrives.
      writer.closed.catch(error => reader.cancel(error).catch(() => {}));
      return (async () => {
        try {
          while (true) {
            const item = await reader.read();
            if (item.done) break;
            await writer.write(item.value);
          }
          await writer.close();
        } catch (error) {
          await Promise.all([reader.cancel(error).catch(() => {}), writer.abort(error).catch(() => {})]);
          throw error;
        } finally { reader.releaseLock(); writer.releaseLock(); }
      })();
    }
    pipeThrough(transform, options = {}) {
      if (!transform || !(transform.readable instanceof ReadableStream) || !(transform.writable instanceof WritableStream))
        throw fail('Expected readable/writable pair');
      if (this.locked || transform.writable.locked) throw fail('Stream is locked');
      if (Object.keys(options).length) throw fail('Pipe options are unsupported');
      this.pipeTo(transform.writable).catch(() => {});
      return transform.readable;
    }
  }
  class ReadableStreamDefaultReader {
    constructor(stream) {
      const state = required(streams, stream);
      if (state.reader) throw fail('Stream is locked');
      const own = { stream, closed: deferred() };
      readers.set(this, own); state.reader = this;
      if (state.status === 'closed') own.closed.resolve();
      if (state.status === 'errored') own.closed.reject(state.error);
    }
    get closed() { return required(readers, this).closed.promise; }
    read() {
      const own = required(readers, this);
      if (!own.stream) return Promise.reject(fail('Reader has released its lock'));
      const state = streams.get(own.stream); state.disturbed = true;
      if (state.status === 'errored') return Promise.reject(state.error);
      if (state.status === 'closed') return Promise.resolve({ value: undefined, done: true });
      const request = deferred(); state.pending.push(request); deliver(state); pull(state);
      return request.promise;
    }
    cancel(reason) {
      const own = required(readers, this);
      return own.stream ? cancelStream(streams.get(own.stream), reason) : Promise.reject(fail('Reader has released its lock'));
    }
    releaseLock() {
      const own = required(readers, this);
      if (!own.stream) return;
      const state = streams.get(own.stream), error = fail('Reader has released its lock');
      while (state.pending.length) state.pending.shift().reject(error);
      if (state.status !== 'readable') own.closed = deferred();
      own.closed.reject(error); state.reader = null; own.stream = null;
    }
  }
  function errorSink(state, error) {
    if (state.status === 'closed' || state.status === 'errored') return;
    state.status = 'errored'; state.error = error; state.closed.reject(error);
    for (const entry of state.pendingWrites) {
      entry.chunk = null;
      if (entry.reserved) { state.queuedBytes -= entry.size; entry.reserved = false; }
      entry.result.reject(error);
    }
    state.pendingWrites.clear();
  }
  class WritableStream {
    constructor(sink = {}, strategy = {}) {
      if (sink.type !== undefined || Object.keys(strategy).length) throw fail('Writable stream strategies are unsupported');
      const state = { sink, status: 'writable', writer: null, chain: Promise.resolve(), closed: deferred(), queuedBytes: 0, pendingWrites: new Set() };
      sinks.set(this, state);
      const controller = { error(error) { errorSink(state, error); } };
      state.chain = Promise.resolve(typeof sink.start === 'function' ? sink.start(controller) : undefined);
      state.chain.catch(error => errorSink(state, error));
    }
    get locked() { return !!required(sinks, this).writer; }
    getWriter() { return new WritableStreamDefaultWriter(this); }
    abort(reason) {
      const state = required(sinks, this);
      if (state.writer) return Promise.reject(fail('Stream is locked'));
      return abortSink(state, reason);
    }
    close() {
      if (this.locked) return Promise.reject(fail('Stream is locked'));
      const writer = this.getWriter();
      return writer.close().finally(() => writer.releaseLock());
    }
  }
  async function abortSink(state, reason) {
    if (state.status === 'closed') return;
    if (state.status === 'errored') return;
    errorSink(state, reason);
    if (typeof state.sink.abort === 'function') await state.sink.abort(reason);
  }
  class WritableStreamDefaultWriter {
    constructor(stream) {
      const state = required(sinks, stream);
      if (state.writer) throw fail('Stream is locked');
      const own = { stream, closed: deferred() };
      state.closed.promise.then(own.closed.resolve, own.closed.reject);
      writers.set(this, own); state.writer = this;
    }
    get closed() {
      return required(writers, this).closed.promise;
    }
    get ready() {
      const own = required(writers, this);
      if (!own.stream) return Promise.reject(fail('Writer has released its lock'));
      const state = sinks.get(own.stream);
      return state.status === 'errored' ? Promise.reject(state.error) : Promise.resolve();
    }
    write(value) {
      const own = required(writers, this);
      if (!own.stream) return Promise.reject(fail('Writer has released its lock'));
      const state = sinks.get(own.stream);
      if (state.status !== 'writable') return Promise.reject(state.status === 'errored' ? state.error : fail('Stream is closing or closed'));
      let chunk;
      try { chunk = byteChunk(value); } catch (error) { return Promise.reject(error); }
      if (state.queuedBytes + chunk.byteLength > LIMIT)
        return Promise.reject(new RangeError('Buffered byte body exceeds 128 MiB limit'));
      state.queuedBytes += chunk.byteLength;
      const entry = { chunk, size: chunk.byteLength, reserved: true, result: deferred() };
      chunk = null;
      state.pendingWrites.add(entry);
      const result = state.chain.then(() => {
        if (state.status === 'errored') throw state.error;
        const bytes = entry.chunk; entry.chunk = null;
        return typeof state.sink.write === 'function' ? state.sink.write(bytes) : undefined;
      }).finally(() => {
        if (entry.reserved) { state.queuedBytes -= entry.size; entry.reserved = false; }
        entry.chunk = null; state.pendingWrites.delete(entry);
      });
      state.chain = result;
      result.then(entry.result.resolve, error => { errorSink(state, error); entry.result.reject(error); });
      return entry.result.promise;
    }
    close() {
      const own = required(writers, this);
      if (!own.stream) return Promise.reject(fail('Writer has released its lock'));
      const state = sinks.get(own.stream);
      if (state.status !== 'writable') return Promise.reject(state.status === 'errored' ? state.error : fail('Stream is closing or closed'));
      state.status = 'closing';
      const result = state.chain.then(async () => {
        if (state.status === 'errored') throw state.error;
        if (typeof state.sink.close === 'function') await state.sink.close();
        if (state.status === 'errored') throw state.error;
        state.status = 'closed'; state.closed.resolve();
      });
      state.chain = result; result.catch(error => errorSink(state, error)); return result;
    }
    abort(reason) {
      const own = required(writers, this);
      return own.stream ? abortSink(sinks.get(own.stream), reason) : Promise.reject(fail('Writer has released its lock'));
    }
    releaseLock() {
      const own = required(writers, this);
      if (own.stream) {
        const state = sinks.get(own.stream);
        if (state.status === 'closed' || state.status === 'errored') own.closed = deferred();
        own.closed.reject(fail('Writer has released its lock'));
        state.writer = null; own.stream = null;
      }
    }
  }
  function fromBytes(bytes) {
    let offset = 0;
    return new ReadableStream({ pull(controller) {
      if (offset === bytes.byteLength) { controller.close(); return; }
      const end = Math.min(offset + 65536, bytes.byteLength);
      controller.enqueue(bytes.subarray(offset, end)); offset = end;
      if (offset === bytes.byteLength) controller.close();
    } });
  }
  function mime(value) {
    const type = value === undefined ? '' : String(value);
    return /[^\x20-\x7e]/.test(type) ? '' : type.toLowerCase();
  }
  class Blob {
    constructor(parts = [], options = {}) {
      if (parts === null || (typeof parts !== 'object' && typeof parts !== 'function')) throw fail('Blob parts must be a sequence');
      if (options.endings !== undefined && options.endings !== 'transparent') throw fail('Native line endings are unsupported');
      const chunks = [];
      for (const part of parts) {
        if (blobs.has(part)) chunks.push(blobs.get(part).bytes);
        else if (part instanceof ArrayBuffer || ArrayBuffer.isView(part)) chunks.push(part);
        else chunks.push(String(part));
      }
      blobs.set(this, { bytes: new Uint8Array(concat(chunks)), type: mime(options.type) });
    }
    get size() { return required(blobs, this).bytes.byteLength; }
    get type() { return required(blobs, this).type; }
    get __isBlob() { required(blobs, this); return true; }
    get __bytes() { return new Uint8Array(copy(required(blobs, this).bytes)); }
    arrayBuffer() { return Promise.resolve(copy(required(blobs, this).bytes)); }
    text() { return Promise.resolve(text(required(blobs, this).bytes)); }
    stream() { return fromBytes(required(blobs, this).bytes); }
    slice(start = 0, end = this.size, type = '') {
      const bytes = required(blobs, this).bytes, size = bytes.byteLength;
      function index(value) {
        const number = Number(value), n = Number.isNaN(number) ? 0 : Math.trunc(number);
        return n < 0 ? Math.max(size + n, 0) : Math.min(n, size);
      }
      const first = index(start), last = Math.max(index(end), first);
      return new Blob([bytes.subarray(first, last)], { type });
    }
  }
  async function consume(response) {
    const own = required(responses, response), body = own.body;
    if (!body) return new ArrayBuffer(0);
    const state = streams.get(body);
    if (own.used || state.disturbed || body.locked) throw fail('Response body is already used or locked');
    own.used = true;
    const reader = body.getReader(), chunks = []; let size = 0;
    try {
      while (true) {
        const item = await reader.read(); if (item.done) break;
        size += item.value.byteLength;
        if (size > LIMIT) { const error = new RangeError('Buffered byte body exceeds 128 MiB limit'); await reader.cancel(error); throw error; }
        chunks.push(item.value);
      }
      return concat(chunks);
    } finally { reader.releaseLock(); }
  }
  class Response {
    constructor(body = null, init = {}) {
      const status = init.status === undefined ? 200 : Number(init.status);
      if (!Number.isInteger(status) || status < 200 || status > 599) throw new RangeError('Invalid response status');
      if (body !== null && [204, 205, 304].includes(status)) throw fail('This status requires a null body');
      const statusText = init.statusText === undefined ? '' : String(init.statusText);
      if (/[\r\n]/.test(statusText)) throw fail('Invalid response status text');
      let stream = null, type = '';
      if (body instanceof ReadableStream) {
        if (body.locked || streams.get(body).disturbed) throw fail('Response body is already used or locked');
        stream = body;
      } else if (body !== null) {
        let bytes;
        if (blobs.has(body)) { const blob = blobs.get(body); bytes = blob.bytes; type = blob.type; }
        else if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) bytes = new Uint8Array(copy(body));
        else { bytes = new Uint8Array(copy(String(body))); type = 'text/plain;charset=UTF-8'; }
        stream = fromBytes(bytes);
      }
      const headers = new globalThis.Headers(init.headers || {});
      if (type && !headers.has('content-type')) headers.set('content-type', type);
      responses.set(this, { body: stream, used: false });
      Object.defineProperties(this, {
        status: { value: status, enumerable: true }, statusText: { value: statusText, enumerable: true },
        ok: { value: status >= 200 && status < 300, enumerable: true },
        headers: { value: headers, enumerable: true }, type: { value: 'default', enumerable: true },
        url: { value: '', enumerable: true }, redirected: { value: false, enumerable: true }
      });
    }
    get body() { return required(responses, this).body; }
    get bodyUsed() { const own = required(responses, this); return own.used || !!(own.body && streams.get(own.body).disturbed); }
    arrayBuffer() { return consume(this); }
    async text() { return text(await consume(this)); }
    async json() { return JSON.parse(await this.text()); }
    async blob() { return new Blob([await consume(this)], { type: this.headers.get('content-type') || '' }); }
  }
  class DecompressionStream {
    constructor(format) {
      format = String(format);
      if (!['gzip', 'deflate', 'deflate-raw'].includes(format)) throw fail('Unsupported compression format');
      let controller, chunks = [], size = 0, cancelled = false;
      const readable = new ReadableStream({
        start(value) { controller = value; },
        cancel() {
          cancelled = true; chunks = [];
          errorSink(sinks.get(writable), fail('Decompression output was cancelled'));
        }
      });
      const writable = new WritableStream({
        write(chunk) {
          if (cancelled) throw fail('Decompression output was cancelled');
          size += chunk.byteLength;
          if (size > LIMIT) { const error = new RangeError('Compressed body exceeds 128 MiB limit'); controller.error(error); throw error; }
          chunks.push(chunk);
        },
        close() {
          try {
            if (cancelled) throw fail('Decompression output was cancelled');
            const output = decompress(concat(chunks), format);
            controller.enqueue(new Uint8Array(output)); controller.close();
          } catch (error) { controller.error(error); throw error; }
          finally { chunks = []; }
        },
        abort(reason) { chunks = []; controller.error(reason); }
      });
      Object.defineProperties(this, { readable: { value: readable, enumerable: true }, writable: { value: writable, enumerable: true } });
    }
  }
  const constructors = { Blob, Response, ReadableStream, ReadableStreamDefaultReader,
    WritableStream, WritableStreamDefaultWriter, DecompressionStream };
  Object.defineProperty(ReadableStream, '__oxiBufferedBytes', { value: true });
  for (const name of names) {
    Object.defineProperty(constructors[name].prototype, Symbol.toStringTag, { value: name, configurable: true });
    globalThis[name] = constructors[name];
    if (globalThis.window) window[name] = constructors[name];
  }
})();
"#;
