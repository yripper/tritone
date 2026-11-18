use super::guard_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn guard_rejects_foreign_host_and_bad_tokens() {
    let app =
        guard_router(axum::Router::new().route("/mcp", axum::routing::post(|| async { "ok" })), 7781, "secret".into());
    let req = |host: &str, auth: Option<&str>| {
        let mut b = Request::post("/mcp").header("host", host);
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        b.body(Body::empty()).unwrap()
    };
    let status = |r: axum::response::Response| r.status();
    assert_eq!(
        status(app.clone().oneshot(req("evil.com:7781", Some("Bearer secret"))).await.unwrap()),
        StatusCode::FORBIDDEN
    );
    assert_eq!(status(app.clone().oneshot(req("127.0.0.1:7781", None)).await.unwrap()), StatusCode::UNAUTHORIZED);
    assert_eq!(
        status(app.clone().oneshot(req("127.0.0.1:7781", Some("Bearer nope"))).await.unwrap()),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(app.clone().oneshot(req("localhost:7781", Some("Bearer secret"))).await.unwrap()),
        StatusCode::OK
    );
    assert_eq!(status(app.oneshot(req("127.0.0.1:7781", Some("Bearer secret"))).await.unwrap()), StatusCode::OK);
}
