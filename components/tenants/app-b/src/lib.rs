//! Tenant **app-b**, function `price-calc`.
//!
//! Looks up a list price in keyvalue and applies a volume discount.
//!
//! Round 2 failure: App B turns on their new `pricing-v2` feature flag. The new
//! discount table only covers sku-1..sku-4, so a sku-7 order indexes past the end,
//! the guest panics, and the component **traps**. Only App B is affected: app-a
//! and app-c run in their own sandboxes.

mod bindings {
    // Reads ./wit/world.wit; packages it uses are vendored in ./wit/deps/.
    wit_bindgen::generate!({ world: "app-b", generate_all });
}

use bindings::exports::demo::tenant::handler::{Guest, Invocation, Reply};
use bindings::wasi::keyvalue::store;
use bindings::wasi::otel::tracing::{self, SpanContext, SpanKind, Status};
use bindings::wasi::otel::types::KeyValue;
use serde::Deserialize;
mod otel;
use otel::kv;

const TENANT: &str = "app-b";
const SERVICE: &str = "tenant-app-b";

#[derive(Deserialize)]
struct Order {
    sku: String,
    qty: u32,
}

struct AppB;

impl Guest for AppB {
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
    let order: Order = serde_json::from_slice(&inv.body).map_err(|e| (400, format!("bad order: {e}")))?;
    attrs.push(kv("order.sku", &order.sku));

    let bucket = store::open(TENANT).map_err(|e| (503, format!("keyvalue unavailable: {e:?}")))?;
    let flags = read_string(&bucket, "flags")?.unwrap_or_default();

    let list_price: u64 = match read_string(&bucket, &format!("price.{}", order.sku))? {
        Some(p) => p.parse().unwrap_or(1000),
        None => {
            // First time we see this SKU: seed a price so the store has data.
            let p = 1000 + 250 * sku_index(&order.sku) as u64;
            bucket.set(&format!("price.{}", order.sku), p.to_string().as_bytes())
                .map_err(|e| (503, format!("keyvalue unavailable: {e:?}")))?;
            p
        }
    };

    let pricing_v2 = flags.split(',').any(|f| f.trim() == "pricing-v2");
    attrs.push(kv("feature.pricing_v2", pricing_v2));

    let discount_pct = if pricing_v2 {
        // pricing-v2: per-SKU discount tiers... for the SKUs someone remembered to add.
        const TIERS: [u64; 4] = [0, 5, 10, 15];
        TIERS[sku_index(&order.sku)] // <- sku-7 panics here: index out of bounds
    } else {
        if order.qty >= 3 { 10 } else { 0 }
    };

    let total = list_price * order.qty as u64 * (100 - discount_pct) / 100;
    otel::log("INFO", ctx, SERVICE, &format!("priced {} x{} = {total}", order.sku, order.qty),
        vec![kv("tenant.id", TENANT)]);

    Ok(serde_json::to_vec(&serde_json::json!({
        "tenant": TENANT, "sku": order.sku, "qty": order.qty,
        "list_price": list_price, "discount_pct": discount_pct, "total": total,
        "pricing": if pricing_v2 { "v2" } else { "v1" },
    })).unwrap())
}

/// "sku-7" -> 7
fn sku_index(sku: &str) -> usize {
    sku.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

fn read_string(bucket: &store::Bucket, key: &str) -> Result<Option<String>, (u16, String)> {
    bucket.get(key)
        .map(|v| v.map(|b| String::from_utf8_lossy(&b).into_owned()))
        .map_err(|e| (503, format!("keyvalue unavailable: {e:?}")))
}

bindings::export!(AppB with_types_in bindings);
