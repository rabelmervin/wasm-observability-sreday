//! Tenant **app-a**, function `invoice-pdf`.
//!
//! Numbers the invoice (keyvalue counter), then asks App A's PDF vendor to render
//! it over outgoing HTTP.
//!
//! Round 1 failure: the vendor gets slow (toxiproxy adds latency). Nothing in
//! *our* code changes, yet app-a's p95 explodes. The `vendor.call` span shows why.

mod bindings {
    // Reads ./wit/world.wit; packages it uses are vendored in ./wit/deps/.
    wit_bindgen::generate!({ world: "app-a", generate_all });
}

use bindings::exports::demo::tenant::handler::{Guest, Invocation, Reply};
use bindings::wasi::config::store as config;
use bindings::wasi::keyvalue::{atomics, store};
use bindings::wasi::otel::tracing::{self, SpanContext, SpanKind, Status};
use bindings::wasi::otel::types::KeyValue;
mod otel;
use otel::kv;

const TENANT: &str = "app-a";
const SERVICE: &str = "tenant-app-a";

struct AppA;

impl Guest for AppA {
    fn handle(inv: Invocation) -> Reply {
        // Parent: the router's span (passed in), else the host's current span.
        let parent = otel::from_traceparent(&inv.traceparent).unwrap_or_else(tracing::outer_span_context);
        let span = otel::start("function.execute", SpanKind::Internal, &parent);
        let ctx = span.ctx.clone();
        otel::install_panic_hook(&ctx, SERVICE, TENANT);
        let mut attrs = vec![kv("tenant.id", TENANT), kv("function.name", &inv.function)];

        match run(&ctx, &mut attrs, &inv) {
            Ok(body) => {
                attrs.push(kv("http.response.status_code", 201));
                otel::end(span, attrs, Status::Ok);
                Reply { status: 201, body }
            }
            Err((status, msg)) => {
                otel::log("ERROR", &ctx, SERVICE, &msg, vec![kv("tenant.id", TENANT)]);
                attrs.push(kv("http.response.status_code", status));
                otel::end(span, attrs, Status::Error(msg.clone()));
                Reply { status, body: serde_json::to_vec(&serde_json::json!({ "error": msg })).unwrap() }
            }
        }
    }
}

fn run(ctx: &SpanContext, attrs: &mut Vec<KeyValue>, inv: &Invocation) -> Result<Vec<u8>, (u16, String)> {
    let bucket = store::open(TENANT).map_err(|e| (503, format!("keyvalue unavailable: {e:?}")))?;
    let number = atomics::increment(&bucket, "invoice-seq", 1)
        .map_err(|e| (503, format!("keyvalue unavailable: {e:?}")))?;
    attrs.push(kv("invoice.number", number));

    let vendor = config::get("vendor_url").ok().flatten()
        .unwrap_or_else(|| "http://toxiproxy:8666/invoices".into());
    let (status, body) = otel::traced_post(ctx, TENANT, &vendor, inv.body.clone(), 5_000)
        .map_err(|e| (502, e))?;
    if status >= 400 {
        return Err((502, format!("pdf vendor returned {status}")));
    }

    let vendor_reply: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    otel::log("INFO", ctx, SERVICE, &format!("invoice #{number} rendered"), vec![kv("tenant.id", TENANT)]);
    Ok(serde_json::to_vec(&serde_json::json!({
        "tenant": TENANT,
        "invoice_number": number,
        "vendor": vendor_reply,
    })).unwrap())
}

bindings::export!(AppA with_types_in bindings);
