//! Platform router: the only component exposed over HTTP.
//!
//!   POST /t/{tenant}/{function}  ->  in-process call into that tenant's component
//!   GET  /healthz
//!
//! Each tenant is a *separate* Wasm component (own sandbox, own memory). The router
//! reaches them through three labeled imports of the same `demo:tenant/handler`
//! interface; wasmCloud v2 links each label to the sibling component of that name.

mod bindings {
    // Reads ./wit/world.wit; packages it uses are vendored in ./wit/deps/.
    wit_bindgen::generate!({ world: "router", generate_all });
}

use bindings::wasi::otel::tracing::{self, SpanKind, Status};
mod otel;
use otel::kv;
use wstd::http::{Body, Request, Response, StatusCode};

const SERVICE: &str = "platform-router";

/// One arm per tenant label. Each label is its own generated module with its own
/// record types, so a macro keeps the dispatch readable.
macro_rules! dispatch {
    ($tenant:expr, $function:expr, $body:expr, $tp:expr; $($label:literal => $module:ident),+ $(,)?) => {
        match $tenant {
            $($label => {
                let reply = bindings::$module::handle(&bindings::$module::Invocation {
                    function: $function.to_string(),
                    body: $body,
                    traceparent: $tp,
                });
                Some((reply.status, reply.body))
            })+
            _ => None,
        }
    };
}

#[wstd::http_server]
async fn main(request: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    let path = request.uri().path().to_string();
    if path == "/healthz" {
        return respond(StatusCode::OK, b"ok".to_vec());
    }

    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let ["t", tenant, function] = segments.as_slice() else {
        return respond(StatusCode::NOT_FOUND, br#"{"error":"use POST /t/{tenant}/{function}"}"#.to_vec());
    };
    let (tenant, function) = (tenant.to_string(), function.to_string());

    let body = request.into_body().contents().await?.to_vec();

    // Parent: the host's HTTP server span. Our span adds the business dimensions
    // (tenant.id, function.name) that the host cannot know about.
    let span = otel::start("tenant.invoke", SpanKind::Internal, &tracing::outer_span_context());
    let ctx = span.ctx.clone();
    let mut attrs = vec![kv("tenant.id", &tenant), kv("function.name", &function)];

    // NOTE: if the tenant traps, this call never returns: the trap unwinds through
    // the router too, and neither guest span is exported. The host still records
    // the failed request. That gap is one of the talk's points.
    let result = dispatch!(tenant.as_str(), function, body, otel::traceparent(&ctx);
        "app-a" => app_a,
        "app-b" => app_b,
        "app-c" => app_c,
    );

    match result {
        Some((status, body)) => {
            attrs.push(kv("http.response.status_code", status));
            if status >= 500 {
                otel::log("ERROR", &ctx, SERVICE, &format!("{tenant}/{function} failed with {status}"),
                    vec![kv("tenant.id", &tenant), kv("function.name", &function)]);
                otel::end(span, attrs, Status::Error(format!("tenant returned {status}")));
            } else {
                otel::end(span, attrs, Status::Ok);
            }
            respond(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body)
        }
        None => {
            attrs.push(kv("http.response.status_code", 404));
            otel::end(span, attrs, Status::Error("unknown tenant".into()));
            respond(StatusCode::NOT_FOUND, br#"{"error":"unknown tenant"}"#.to_vec())
        }
    }
}

fn respond(status: StatusCode, body: Vec<u8>) -> Result<Response<Body>, wstd::http::Error> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .map_err(Into::into)
}
