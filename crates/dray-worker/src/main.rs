//! The worker binary.
//!
//! Thin over [`dray_worker`] (ADR-006): everything of substance is in the
//! library so the integration tests can drive a worker in-process, which is the
//! only way to kill one mid-proof and watch what the store does about it.

use std::process::ExitCode;

use std::sync::Arc;

use dray_worker::{
    config::Config,
    metrics::Metrics,
    prover::{self, ProverConfig},
    worker::{ShutdownHandle, Worker, shutdown},
};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "worker exited");
            eprintln!("dray-worker: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    init_tracing(&config);

    tracing::info!(
        worker = %config.worker.worker_id,
        circuits = ?config.circuits,
        "starting"
    );

    let store = dray_store::Store::connect(&config.database_url, config.max_db_connections).await?;

    // Artefacts are prepared before the first lease, not lazily on the first
    // job. A worker that discovered a broken toolchain only once it held a job
    // would fail that job for a reason that has nothing to do with it.
    let mut prover_config = ProverConfig::new(
        prover::Artifacts::at(&config.artifacts_dir),
        config.scratch_dir.clone(),
    );
    prover_config.bounds = config.bounds;

    std::fs::create_dir_all(&config.scratch_dir)?;
    std::fs::create_dir_all(&config.artifacts_dir)?;

    tracing::info!(dir = %config.artifacts_dir.display(), "preparing circuit artefacts");
    prover_config.artifacts = prover::prepare(
        &config.circuits_dir,
        &config.circuits,
        &config.artifacts_dir,
        &prover_config,
    )
    .await?;

    let (handle, signal) = shutdown();
    let signals = tokio::spawn(await_signals(handle));

    let worker = Worker::new(store, config.worker.clone(), prover_config);

    // Serving metrics from the worker process rather than a sidecar keeps the
    // numbers and the thing they describe in the same lifetime: if the worker
    // is gone, the scrape fails, which is itself the signal.
    let metrics = tokio::spawn(serve_metrics(config.metrics_bind.clone(), worker.metrics()));

    let outcomes = worker.run(signal).await;

    metrics.abort();
    signals.abort();

    tracing::info!(attempts = outcomes.len(), "stopped cleanly");
    Ok(())
}

/// Translate SIGTERM and SIGINT into a shutdown request.
///
/// SIGTERM is what an orchestrator sends before it eventually resorts to
/// SIGKILL. Handling it is what turns a deploy from "every in-flight job waits
/// out its lease" into "every in-flight job is finished or handed straight
/// back".
async fn await_signals(handle: ShutdownHandle) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                tracing::error!(error = %err, "cannot listen for SIGTERM");
                return;
            }
        };

        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM received; shutting down"),
            result = tokio::signal::ctrl_c() => {
                match result {
                    Ok(()) => tracing::info!("interrupt received; shutting down"),
                    Err(err) => tracing::error!(error = %err, "cannot listen for interrupts"),
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "cannot listen for interrupts");
            return;
        }
        tracing::info!("interrupt received; shutting down");
    }

    handle.trigger();
}

/// Serve `GET /metrics` for Prometheus to scrape.
///
/// A failure to bind is logged and tolerated rather than fatal. Losing
/// observability is bad; refusing to prove anything because a metrics port was
/// already taken would be worse.
async fn serve_metrics(bind: String, metrics: Arc<Metrics>) {
    use axum::{Router, extract::State, routing::get};

    let app = Router::new()
        .route(
            "/metrics",
            get(|State(metrics): State<Arc<Metrics>>| async move {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; version=0.0.4; charset=utf-8",
                    )],
                    metrics.render(),
                )
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .with_state(metrics);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(bind = %bind, error = %err, "could not serve metrics");
            return;
        }
    };

    tracing::info!(bind = %bind, "serving metrics");
    if let Err(err) = axum::serve(listener, app).await {
        tracing::error!(error = %err, "the metrics server stopped");
    }
}

fn init_tracing(config: &Config) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("dray={}", config.log_level)));

    if config.log_json {
        fmt().json().with_env_filter(filter).init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}
