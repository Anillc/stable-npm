//! The registry base is `/<time>/`: npm is pointed at `http://host/<time>/` and
//! appends package names itself. Tarball URLs keep pointing at
//! registry.npmjs.org.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use axum::routing::{any, get};
use percent_encoding::percent_decode_str;

use crate::service::{Answer, Service};
use crate::timeutil::{now_ms, parse_instant};

const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

const EMPTY_AUDIT_REPORT: &str = concat!(
    r#"{"auditReportVersion":2,"vulnerabilities":{},"metadata":{"vulnerabilities":"#,
    r#"{"info":0,"low":0,"moderate":0,"high":0,"critical":0,"total":0},"dependencies":"#,
    r#"{"prod":0,"dev":0,"optional":0,"peer":0,"peerOptional":0,"total":0}}}"#
);

pub fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/", get(usage))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(any(fallback))
        .with_state(service)
}

async fn usage() -> Response {
    let text = "\
stable-npm - time-traveling read-only npm registry proxy

Point npm at a registry base that includes the instant you want:

    npm install --registry http://127.0.0.1:3000/2020-01-01T00:00:00Z/ <pkg>

Endpoints:
    GET /<time>/<name>            full packument as of <time>
    GET /<time>/<name>/<version>  single version manifest as of <time>
    GET /healthz                  liveness probe

<time> is RFC3339 in UTC, e.g. 2020-01-01T00:00:00Z. It must not be in the
future. The registry base must end with a slash.

Tarballs are never proxied or stored: we 302 them to registry.npmjs.org. To
skip the round trip, tell npm to leave tarball URLs alone:

    npm config set replace-registry-host never
";
    raw_response(StatusCode::OK, "text/plain; charset=utf-8", text)
}

async fn fallback(
    State(service): State<Arc<Service>>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Response {
    let Some((time_str, rest_raw)) = uri.path().strip_prefix('/').and_then(|p| p.split_once('/'))
    else {
        return bad_request("expected /<time>/<package>");
    };

    let Some(t_ms) = parse_instant(time_str) else {
        return bad_request("invalid <time>: expected RFC3339, e.g. 2020-01-01T00:00:00Z");
    };
    if t_ms > now_ms() {
        return bad_request("<time> must not be in the future");
    }

    let rest = percent_decode_str(rest_raw).decode_utf8_lossy().to_string();

    // Registry meta-namespace: /-/ping, /-/npm/v1/security/...
    if rest == "-" || rest.starts_with("-/") {
        return registry_namespace(&rest);
    }

    // npm rewrites registry.npmjs.org tarball URLs onto the configured
    // registry. We do not proxy or store tarballs, so bounce them back.
    if rest.contains("/-/") {
        let location = service.tarball_location(rest_raw);
        return redirect(&location);
    }

    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return raw_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "application/json",
            r#"{"error":"method not allowed"}"#,
        );
    }
    let if_none_match = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let (name, version) = match parse_spec(&rest) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(message),
    };

    let answer = match version {
        Some(version) => service.version_manifest(&name, &version, t_ms).await,
        None => service.packument(&name, t_ms).await,
    };

    match answer {
        Ok(answer) => respond(answer, method == Method::HEAD, if_none_match.as_deref()),
        Err(error) => {
            tracing::warn!(package = %name, error = %error, "request failed");
            raw_response(
                StatusCode::BAD_GATEWAY,
                "application/json",
                r#"{"error":"upstream error"}"#,
            )
        }
    }
}

/// npm's audit/ping endpoints: answer empty instead of failing the install.
fn registry_namespace(rest: &str) -> Response {
    if rest.contains("/security/audits") {
        raw_response(StatusCode::OK, "application/json", EMPTY_AUDIT_REPORT)
    } else {
        raw_response(StatusCode::OK, "application/json", "{}")
    }
}

fn redirect(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .expect("valid response")
}

fn respond(answer: Answer, head_only: bool, if_none_match: Option<&str>) -> Response {
    if let Some(if_none_match) = if_none_match
        && if_none_match
            .split(',')
            .any(|tag| tag.trim() == answer.etag || tag.trim() == "*")
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &answer.etag)
            .header(header::CACHE_CONTROL, CACHE_CONTROL)
            .body(Body::empty())
            .expect("valid response");
    }

    let body = if head_only {
        Body::empty()
    } else {
        Body::from(answer.body)
    };
    Response::builder()
        .status(StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ETAG, answer.etag)
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .body(body)
        .expect("valid response")
}

fn bad_request(message: &str) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    raw_response(StatusCode::BAD_REQUEST, "application/json", &body)
}

fn raw_response(status: StatusCode, content_type: &str, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body.to_string()))
        .expect("valid response")
}

fn parse_spec(rest: &str) -> Result<(String, Option<String>), &'static str> {
    if rest.is_empty() {
        return Err("empty package name");
    }
    let parts: Vec<&str> = rest.split('/').collect();
    if parts[0].is_empty() {
        return Err("empty package name");
    }

    let (name, version) = if parts[0].starts_with('@') {
        if parts.len() < 2 || parts[1].is_empty() {
            return Err("scoped package must be @scope/name");
        }
        if parts.len() > 3 {
            return Err("too many path segments");
        }
        (format!("{}/{}", parts[0], parts[1]), parts.get(2).copied())
    } else {
        if parts.len() > 2 {
            return Err("too many path segments");
        }
        (parts[0].to_string(), parts.get(1).copied())
    };

    let version = version.filter(|v| !v.is_empty()).map(str::to_string);
    Ok((name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_scoped_specs() {
        assert_eq!(parse_spec("express"), Ok(("express".into(), None)));
        assert_eq!(
            parse_spec("express/4.18.2"),
            Ok(("express".into(), Some("4.18.2".into())))
        );
        assert_eq!(parse_spec("@types/node"), Ok(("@types/node".into(), None)));
        assert_eq!(
            parse_spec("@types/node/20.0.0"),
            Ok(("@types/node".into(), Some("20.0.0".into())))
        );
    }

    #[test]
    fn rejects_malformed_specs() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("@scope").is_err());
        assert!(parse_spec("a/b/c").is_err());
        assert!(parse_spec("@s/n/v/x").is_err());
    }
}
