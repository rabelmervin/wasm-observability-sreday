//! Minimal helper over `wasi:otel`. App code uses the wasi:otel types directly
//! (`SpanContext`, `KeyValue`, `Status`, `outer_span_context()`); this file only does
//! what the interface leaves to the guest: new IDs, timestamps, and filling the
//! 13-field `SpanData` when a span ends. The host batches and exports OTLP.

use std::fmt::Write as _;

use crate::bindings::wasi::clocks::wall_clock::{self, Datetime};
use crate::bindings::wasi::otel::logs;
use crate::bindings::wasi::otel::tracing::{self, SpanContext, SpanKind, Status, TraceFlags};
use crate::bindings::wasi::otel::types::{InstrumentationScope, KeyValue};
use crate::bindings::wasi::random::random::get_random_bytes;

/// A span that has been started; `end()` needs these values again.
pub struct Started {
    pub ctx: SpanContext,
    parent_span_id: String,
    name: &'static str,
    kind: SpanKind,
    start: Datetime,
}

/// Start a child of `parent` (an empty parent starts a new trace).
pub fn start(name: &'static str, kind: SpanKind, parent: &SpanContext) -> Started {
    let new_trace = parent.trace_id.is_empty();
    let ctx = SpanContext {
        trace_id: if new_trace { hex(16) } else { parent.trace_id.clone() },
        span_id: hex(8),
        trace_flags: if new_trace { TraceFlags::SAMPLED } else { parent.trace_flags },
        is_remote: false,
        trace_state: parent.trace_state.clone(),
    };
    tracing::on_start(&ctx);
    Started { ctx, parent_span_id: parent.span_id.clone(), name, kind, start: wall_clock::now() }
}

/// End a span: hand the finished span to the host.
pub fn end(span: Started, attributes: Vec<KeyValue>, status: Status) {
    let events = match &status {
        Status::Error(msg) => vec![tracing::Event {
            name: "exception".into(),
            time: wall_clock::now(),
            attributes: vec![kv("exception.message", msg)],
        }],
        _ => vec![],
    };
    tracing::on_end(&tracing::SpanData {
        span_context: span.ctx,
        parent_span_id: span.parent_span_id,
        span_kind: span.kind,
        name: span.name.into(),
        start_time: span.start,
        end_time: wall_clock::now(),
        attributes,
        events,
        links: vec![],
        status,
        instrumentation_scope: scope(),
        dropped_attributes: 0,
        dropped_events: 0,
        dropped_links: 0,
    });
}

/// Attribute. wasmCloud 2.12 forwards values as raw strings (not JSON).
pub fn kv(key: &str, value: impl ToString) -> KeyValue {
    KeyValue { key: key.into(), value: value.to_string() }
}

/// Log record tied to `ctx` (Loki gets trace_id/span_id). severity: INFO | WARN | ERROR.
pub fn log(severity: &str, ctx: &SpanContext, service: &str, msg: &str, attributes: Vec<KeyValue>) {
    logs::on_emit(&logs::LogRecord {
        timestamp: Some(wall_clock::now()),
        observed_timestamp: None,
        severity_text: Some(severity.into()),
        severity_number: Some(match severity { "ERROR" => 17, "WARN" => 13, _ => 9 }),
        body: Some(msg.into()),
        attributes: Some(attributes),
        event_name: None,
        resource: Some(logs::Resource { attributes: vec![kv("service.name", service)], schema_url: None }),
        instrumentation_scope: Some(scope()),
        trace_id: Some(ctx.trace_id.clone()),
        span_id: Some(ctx.span_id.clone()),
        trace_flags: Some(ctx.trace_flags),
    });
}

fn scope() -> InstrumentationScope {
    InstrumentationScope { name: "sreday-demo".into(), version: None, schema_url: None, attributes: vec![] }
}

fn hex(bytes: u64) -> String {
    let mut out = String::new();
    for b in get_random_bytes(bytes) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Parse a W3C `traceparent` (`00-<trace>-<span>-<flags>`) into a remote parent.
pub fn from_traceparent(tp: &str) -> Option<SpanContext> {
    let mut parts = tp.split('-');
    let (_version, trace_id, span_id, flags) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if trace_id.len() != 32 || span_id.len() != 16 {
        return None;
    }
    let sampled = u8::from_str_radix(flags, 16).ok()? & 1 == 1;
    Some(SpanContext {
        trace_id: trace_id.into(),
        span_id: span_id.into(),
        trace_flags: if sampled { TraceFlags::SAMPLED } else { TraceFlags::empty() },
        is_remote: true,
        trace_state: vec![],
    })
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<(SpanContext, &'static str, &'static str)>> =
        const { std::cell::RefCell::new(None) };
}

/// A panic in a Wasm guest becomes a trap: the instance dies and in-flight spans are
/// never ended. This hook runs *before* the trap and gets one last ERROR log and a
/// `guest.panic` span out of the sandbox, tied to the current trace.
pub fn install_panic_hook(ctx: &SpanContext, service: &'static str, tenant: &'static str) {
    CURRENT.with(|c| *c.borrow_mut() = Some((ctx.clone(), service, tenant)));
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        eprintln!("{msg}");
        CURRENT.with(|c| {
            if let Some((ctx, service, tenant)) = c.borrow().as_ref() {
                log("ERROR", ctx, service, &format!("guest panic -> wasm trap: {msg}"), vec![
                    kv("tenant.id", tenant),
                    kv("exception.type", "panic"),
                    kv("exception.message", &msg),
                ]);
                let span = start("guest.panic", SpanKind::Internal, ctx);
                end(span, vec![kv("tenant.id", tenant)], Status::Error(msg.clone()));
            }
        });
    }));
}
