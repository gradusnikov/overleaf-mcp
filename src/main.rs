//! overleaf-mcp: an MCP server (streamable HTTP) for an Overleaf Community Edition instance.
//!
//! Configuration (environment):
//! - `OVERLEAF_URL`            base URL of Overleaf, e.g. `http://sharelatex` inside the compose network
//! - `OVERLEAF_EMAIL`          account the server acts as
//! - `OVERLEAF_PASSWORD` / `OVERLEAF_PASSWORD_FILE`
//! - `MCP_TOKENS` / `MCP_TOKENS_FILE`  bearer tokens accepted from clients (comma- or line-separated)
//! - `MCP_BIND`                listen address (default `0.0.0.0:8080`)
//! - `MCP_ALLOWED_HOSTS`       accepted `Host` headers, comma-separated, `*` for any (default: loopback only)
//! - `MCP_TLS_CERT`, `MCP_TLS_KEY`  PEM files; when set the server speaks HTTPS and reloads them every 12 h

mod lines;
mod overleaf;
mod tools;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get};
use axum_server::tls_rustls::RustlsConfig;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::overleaf::Overleaf;
use crate::tools::OverleafMcp;

const CERT_RELOAD_INTERVAL: Duration = Duration::from_secs(12 * 3600);

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Reads `NAME`, or the content of the file named by `NAME_FILE`.
fn secret(name: &str) -> Result<Option<String>> {
    if let Some(value) = env(name) {
        return Ok(Some(value));
    }
    match env(&format!("{name}_FILE")) {
        Some(path) => {
            let value = std::fs::read_to_string(&path).with_context(|| format!("reading {name}_FILE {path}"))?;
            Ok(Some(value.trim().to_string()))
        }
        None => Ok(None),
    }
}

fn required(name: &str) -> Result<String> {
    secret(name)?.with_context(|| format!("{name} (or {name}_FILE) must be set"))
}

fn parse_tokens(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty() && !t.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_bearer(State(tokens): State<Arc<Vec<String>>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let authorized = presented.is_some_and(|t| tokens.iter().any(|k| constant_time_eq(k.as_bytes(), t.as_bytes())));
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized\n",
        )
            .into_response();
    }
    next.run(request).await
}

async fn shutdown_signal(token: CancellationToken) {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    token.cancel();
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,rmcp=warn".into()),
        )
        .init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("cannot install the rustls crypto provider"))?;

    let overleaf_url = env("OVERLEAF_URL").unwrap_or_else(|| "http://sharelatex".to_string());
    let overleaf = Arc::new(Overleaf::new(
        &overleaf_url,
        required("OVERLEAF_EMAIL")?,
        required("OVERLEAF_PASSWORD")?,
    )?);
    let tokens = Arc::new(parse_tokens(&required("MCP_TOKENS")?));
    if tokens.is_empty() {
        bail!("MCP_TOKENS contains no token");
    }
    let bind: SocketAddr = env("MCP_BIND")
        .unwrap_or_else(|| "0.0.0.0:8080".to_string())
        .parse()
        .context("MCP_BIND")?;

    let shutdown = CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default().with_cancellation_token(shutdown.child_token());
    match env("MCP_ALLOWED_HOSTS").as_deref() {
        Some("*") => config = config.disable_allowed_hosts(),
        Some(hosts) => config = config.with_allowed_hosts(hosts.split(',').map(str::trim).filter(|h| !h.is_empty())),
        None => {}
    }
    let service = StreamableHttpService::new(
        {
            let overleaf = overleaf.clone();
            move || Ok(OverleafMcp::new(overleaf.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(tokens.clone(), require_bearer))
        .route("/healthz", get(|| async { "ok\n" }));

    tokio::spawn(shutdown_signal(shutdown.clone()));
    tracing::info!(%bind, overleaf = %overleaf_url, tokens = tokens.len(), "overleaf-mcp listening");

    match (env("MCP_TLS_CERT"), env("MCP_TLS_KEY")) {
        (Some(cert), Some(key)) => {
            let tls = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .context("loading MCP_TLS_CERT/MCP_TLS_KEY")?;
            tokio::spawn({
                let tls = tls.clone();
                async move {
                    loop {
                        tokio::time::sleep(CERT_RELOAD_INTERVAL).await;
                        match tls.reload_from_pem_file(&cert, &key).await {
                            Ok(()) => tracing::info!("TLS certificate reloaded"),
                            Err(e) => tracing::warn!(error = %e, "TLS certificate reload failed"),
                        }
                    }
                }
            });
            let handle = axum_server::Handle::new();
            tokio::spawn({
                let handle = handle.clone();
                let shutdown = shutdown.clone();
                async move {
                    shutdown.cancelled().await;
                    handle.graceful_shutdown(Some(Duration::from_secs(10)));
                }
            });
            axum_server::bind_rustls(bind, tls)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(bind).await?;
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
                .await?;
        }
        _ => bail!("set both MCP_TLS_CERT and MCP_TLS_KEY, or neither"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_parsed() {
        assert_eq!(parse_tokens("a, b\n# comment\n\nc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn tokens_compare() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
    }
}
