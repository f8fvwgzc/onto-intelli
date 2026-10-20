//! The operator web UI, embedded in the binary and served at `/ui`.
//!
//! The page is static; it calls the same HTTP API with the same tokens. It
//! can be embedded in an iframe by the origins in `[ui] embed_origins` (CSP
//! `frame-ancestors`), which may also hand it a session with `postMessage`.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde_json::json;

mod assets {
    include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));
}

#[derive(Clone)]
struct UiState {
    title: String,
    embed_origins: Vec<String>,
    csp: HeaderValue,
}

/// Is a built UI embedded in this binary?
pub fn bundled() -> bool {
    assets::FILES.iter().any(|(p, _)| *p == "index.html")
}

pub fn router(cfg: &onto_intelli::config::UiConfig) -> axum::Router {
    let mut ancestors = vec!["'self'".to_string()];
    ancestors.extend(cfg.embed_origins.iter().filter(|o| valid_origin(o)).cloned());
    let csp = format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         font-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
         frame-ancestors {}",
        ancestors.join(" ")
    );
    let state = UiState {
        title: cfg.title.clone(),
        embed_origins: cfg.embed_origins.iter().filter(|o| valid_origin(o)).cloned().collect(),
        csp: HeaderValue::from_str(&csp).unwrap_or_else(|_| HeaderValue::from_static("frame-ancestors 'self'")),
    };
    axum::Router::new()
        .route("/", get(|| async { Redirect::temporary("/ui/") }))
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/", get(index))
        .route("/ui/config.json", get(config))
        .route("/ui/{*path}", get(file))
        .with_state(state)
}

/// `https://host[:port]` or `http://localhost[:port]`: nothing that could
/// widen the CSP (no wildcards, paths or keywords).
fn valid_origin(o: &str) -> bool {
    let rest = o.strip_prefix("https://").or_else(|| o.strip_prefix("http://"));
    rest.is_some_and(|r| {
        !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    })
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

fn respond(s: &UiState, path: &str, body: &'static [u8]) -> Response {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime(path)));
    let immutable = path.starts_with("assets/");
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "no-cache" }),
    );
    h.insert(header::CONTENT_SECURITY_POLICY, s.csp.clone());
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    (StatusCode::OK, h, body).into_response()
}

fn find(path: &str) -> Option<&'static [u8]> {
    assets::FILES.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
}

async fn index(State(s): State<UiState>) -> Response {
    match find("index.html") {
        Some(b) => respond(&s, "index.html", b),
        None => (
            StatusCode::NOT_FOUND,
            "The web UI is not built into this binary: run `npm ci && npm run build` in ui/, then rebuild.",
        )
            .into_response(),
    }
}

async fn config(State(s): State<UiState>) -> Response {
    let body = json!({ "title": s.title, "embed_origins": s.embed_origins, "version": onto_intelli::VERSION });
    let mut r = axum::Json(body).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

async fn file(State(s): State<UiState>, Path(path): Path<String>) -> Response {
    match find(&path) {
        Some(b) => respond(&s, &path, b),
        // Client-side routes fall back to the app shell; missing assets 404.
        None if path.starts_with("assets/") => StatusCode::NOT_FOUND.into_response(),
        None => index(State(s)).await,
    }
}

#[cfg(test)]
mod tests {
    use super::valid_origin;

    #[test]
    fn embed_origins_cannot_widen_the_policy() {
        assert!(valid_origin("https://nexc.example.com"));
        assert!(valid_origin("http://localhost:5173"));
        assert!(!valid_origin("*"));
        assert!(!valid_origin("https://*.example.com"));
        assert!(!valid_origin("https://a.com 'unsafe-inline'"));
        assert!(!valid_origin("javascript:alert(1)"));
    }
}
