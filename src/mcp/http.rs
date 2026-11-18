//! Streamable HTTP on 127.0.0.1 behind a Host check and a bearer token.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::post;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::server::TritoneServer;
use super::state::AppState;
use crate::secrets::ACCOUNT_TOKEN;

#[derive(Clone)]
struct Guard {
    hosts: [String; 2],
    expected_auth: Arc<str>,
}

/// Equal-time comparison so the token can't be guessed byte by byte from timing.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn guard(State(g): State<Guard>, req: Request, next: Next) -> Result<Response, StatusCode> {
    // A malicious page can rebind its own hostname to 127.0.0.1, but the
    // browser still sends that hostname in Host, so only loopback Host values pass.
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if !g.hosts.iter().any(|h| h == host) {
        return Err(StatusCode::FORBIDDEN);
    }
    let auth = req.headers().get(header::AUTHORIZATION).map(|h| h.as_bytes()).unwrap_or(b"");
    // Fail closed: an empty token would accept the literal header "Bearer ".
    if g.expected_auth.len() <= "Bearer ".len() || !same(auth, g.expected_auth.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

pub fn guard_router(router: Router, port: u16, token: String) -> Router {
    let g = Guard {
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        expected_auth: format!("Bearer {token}").into(),
    };
    router.layer(middleware::from_fn_with_state(g, guard))
}

/// The guarded MCP endpoint at `/mcp`, and `POST /login-notify` for `tritone login`.
pub fn app(server: TritoneServer, port: u16, token: String) -> Router {
    let notify = Router::new().route("/login-notify", post(login_notify)).with_state(server.state.clone());
    // Stateless: no session state can leak between Claude clients.
    let cfg = StreamableHttpServerConfig::default().with_legacy_session_mode(false).with_json_response(true);
    let svc = StreamableHttpService::new(move || Ok(server.clone()), Arc::new(NeverSessionManager::default()), cfg);
    guard_router(Router::new().nest_service("/mcp", svc).merge(notify), port, token)
}

/// `tritone login` calls this after saving a token, so the new login takes
/// effect even if nobody polls `status`. Same rule as `status`: only a token
/// Discord hasn't rejected (nor been tried since the halt) wakes the gateway,
/// so repeating the call can't drive an identify loop.
async fn login_notify(State(state): State<AppState>) -> (StatusCode, &'static str) {
    match state.secrets.get(ACCOUNT_TOKEN).await {
        Ok(Some(token)) if state.live.try_login_with(&token) => {
            state.login_notify.notify_one();
            tracing::info!("new login; reconnecting");
            (StatusCode::OK, "tritone is reconnecting with the new login")
        }
        Ok(Some(_)) => (
            StatusCode::CONFLICT,
            "tritone already tried this login (or Discord rejected it): run `tritone login` again",
        ),
        Ok(None) => (StatusCode::CONFLICT, "there's no Discord login in the keyring: run `tritone login`"),
        Err(e) => {
            tracing::warn!("reading the keyring failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "tritone could not read the keyring")
        }
    }
}

pub async fn serve(server: TritoneServer, listener: tokio::net::TcpListener, token: String) -> anyhow::Result<()> {
    let port = listener.local_addr()?.port();
    axum::serve(listener, app(server, port, token))
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {},
            }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
