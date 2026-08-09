use rbpmn_server::{DEFAULT_BIND, Tokens, app, validate_bind};
use std::net::SocketAddr;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let bind_raw = std::env::var("RBPMN_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let bind: SocketAddr = match bind_raw.parse() {
        Ok(addr) => addr,
        Err(_) => {
            eprintln!("invalid RBPMN_BIND '{bind_raw}': expected host:port, e.g. {DEFAULT_BIND}");
            return ExitCode::from(2);
        }
    };

    let allow_non_loopback = std::env::var("RBPMN_ALLOW_NON_LOOPBACK")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    if let Err(msg) = validate_bind(&bind, allow_non_loopback) {
        eprintln!("{msg}");
        return ExitCode::from(2);
    }

    let tokens = match Tokens::from_env() {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {bind}: {e}");
            return ExitCode::from(1);
        }
    };

    tracing::info!(%bind, tokens = tokens.len(), "rbpmn-server listening");

    let result = axum::serve(listener, app(tokens))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server error: {e}");
            ExitCode::from(1)
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
