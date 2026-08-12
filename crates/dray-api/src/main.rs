//! Ingest HTTP API.
//!
//! Accepts proof requests, validates them against the target circuit's declared
//! input schema, canonicalises and hashes the inputs for deduplication, and
//! enqueues durable jobs. See `DRAY_BUILD_SPEC.md` §5 Phase 2.

use dray_api::{api, config::Config};
use dray_store::Store;
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    init_tracing(&config);

    tracing::info!(
        component = dray_api::COMPONENT,
        version = env!("CARGO_PKG_VERSION"),
        bind = %config.bind_address,
        "starting"
    );

    let store = Store::connect(&config.database_url, config.max_db_connections).await?;

    // Applying migrations on start is a development convenience. In production
    // this should be a deliberate step in the deploy, not a side effect of a
    // process starting — several replicas racing to migrate is not a situation
    // worth engineering around when a single explicit step avoids it.
    if config.migrate_on_start {
        tracing::info!("applying migrations");
        store.migrate().await?;
    }

    let state = api::AppState {
        store,
        limits: config.limits,
        max_queue_depth: config.max_queue_depth,
        default_max_attempts: config.default_max_attempts,
    };

    let app = api::router(state)
        // Request ids are generated here if the client did not supply one, and
        // echoed back either way, so a client can quote an id when reporting a
        // problem and it will match the server's logs.
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(config.limits.max_body_bytes))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    tracing::info!(address = %listener.local_addr()?, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shut down cleanly");
    Ok(())
}

fn init_tracing(config: &Config) {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    let registry = tracing_subscriber::registry().with(filter);

    // Structured JSON in production so logs are queryable; human-readable
    // locally so they are readable.
    if config.log_json {
        registry
            .with(fmt::layer().json().with_current_span(true))
            .init();
    } else {
        registry.with(fmt::layer().compact()).init();
    }
}

/// Waits for SIGTERM or Ctrl-C.
///
/// Graceful shutdown matters here for the same reason it matters in the worker:
/// a rolling deploy should drain in-flight requests rather than dropping them
/// and making clients retry work that was about to succeed.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "could not install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("received Ctrl-C, draining"),
        () = terminate => tracing::info!("received SIGTERM, draining"),
    }
}
