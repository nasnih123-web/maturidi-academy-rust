use std::net::SocketAddr;

use maturidi_api_rs::{build_router, AppState, Config};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    // Storage boot validation (ADR-162): in production a bad storage
    // config refuses to start — the 2026-11-19 receipt loss was a
    // local-disk deployment, and the binary must never boot into one
    // silently. Dev/test never refuse (the validator returns empty
    // there).
    let storage_problems = maturidi_api_rs::storage::validate_storage_config(&config);
    if !storage_problems.is_empty() {
        eprintln!("storage configuration problems (production):");
        for problem in &storage_problems {
            eprintln!("  - {problem}");
        }
        std::process::exit(1);
    }
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("failed to bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    tracing::info!(
        "maturidi-api-rs listening on {addr} (env={})",
        config.app_env
    );
    let state = AppState::new(config).await;
    let service = build_router(state).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(err) = axum::serve(listener, service).await {
        eprintln!("server error: {err}");
        std::process::exit(1);
    }
}
