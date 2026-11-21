//! Tenant **app-c**, function `data-sync`.
//!
//! Pushes a batch of records to App C's CRM vendor and stores a sync cursor.
//!
//! Round 3 failure (noisy neighbour): App C turns on `full-resync`, which loads
//! the *whole* dataset into memory on every call instead of one batch. Each
//! instance grows its linear memory by hundreds of MiB. The sandbox isolates
//! memory *contents*, but the host's memory budget is shared.

mod bindings {
    // Reads ./wit/world.wit; packages it uses are vendored in ./wit/deps/.
    wit_bindgen::generate!({ world: "app-c", generate_all });
}

use bindings::exports::demo::tenant::handler::{Guest, Invocation, Reply};
use bindings::wasi::config::store as config;
use bindings::wasi::keyvalue::store;
use bindings::wasi::otel::tracing::{self, SpanContext, SpanKind, Status};
use bindings::wasi::otel::types::KeyValue;
mod otel;
use otel::kv;

const TENANT: &str = "app-c";
const SERVICE: &str = "tenant-app-c";

struct AppC;

impl Guest for AppC {
    fn handle(inv: Invocation) -> Reply {
        // Parent: the router's span (passed in), else the host's current span.
        let parent = otel::from_traceparent(&inv.traceparent).unwrap_or_else(tracing::outer_span_context);
        let span = otel::start("function.execute", SpanKind::Internal, &parent);
        let ctx = span.ctx.clone();
        otel::install_panic_hook(&ctx, SERVICE, TENANT);
        let mut attrs = vec![kv("tenant.id", TENANT), kv("function.name", &inv.function)];

        match run(&ctx, &mut attrs, &inv) {
            Ok(body) => {
                attrs.push(kv("http.response.status_code", 200));
                otel::end(span, attrs, Status::Ok);
                Reply { status: 200, body }
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
    let flags = bucket.get("flags")
        .map_err(|e| (503, format!("keyvalue unavailable: {e:?}")))?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let full_resync = flags.split(',').any(|f| f.trim() == "full-resync");
    attrs.push(kv("feature.full_resync", full_resync));

    // The dataset we hold in memory while talking to the vendor.
    let dataset: Vec<u8> = if full_resync {
        let mb: usize = config::get("resync_mb").ok().flatten()
            .and_then(|v| v.parse().ok()).unwrap_or(384);
        attrs.push(kv("sync.dataset_mb", mb));
        otel::log("WARN", ctx, SERVICE, &format!("full resync: loading {mb} MiB dataset"),
            vec![kv("tenant.id", TENANT)]);
        // Ask for the memory fallibly: when the host's guest-memory budget refuses
        // the growth we get an error we can trace, instead of an abort (an
        // allocation failure aborts *without* running the panic hook).
        let bytes = mb * 1024 * 1024;
        let mut data = Vec::new();
        if data.try_reserve_exact(bytes).is_err() {
            attrs.push(kv("memory.refused_mb", mb));
            return Err((507, format!("host refused memory growth: full resync needs {mb} MiB")));
        }
        // Touch every page so the memory is really committed.
        data.resize(bytes, 0xA5u8);
        data
    } else {
        inv.body.clone()
    };

    let vendor = config::get("vendor_url").ok().flatten()
        .unwrap_or_else(|| "http://toxiproxy:8667/sync".into());
    let checksum = dataset.iter().step_by(4096).fold(0u64, |a, b| a.wrapping_add(*b as u64));
    let payload = serde_json::to_vec(&serde_json::json!({ "bytes": dataset.len(), "checksum": checksum })).unwrap();

    let (status, _) = otel::traced_post(ctx, TENANT, &vendor, payload, 5_000).map_err(|e| (502, e))?;
    if status >= 400 {
        return Err((502, format!("crm vendor returned {status}")));
    }

    bucket.set("cursor", checksum.to_string().as_bytes())
        .map_err(|e| (503, format!("keyvalue unavailable: {e:?}")))?;
    drop(dataset);

    otel::log("INFO", ctx, SERVICE, "sync complete", vec![kv("tenant.id", TENANT)]);
    Ok(serde_json::to_vec(&serde_json::json!({ "tenant": TENANT, "synced": true, "full_resync": full_resync })).unwrap())
}

bindings::export!(AppC with_types_in bindings);
