//! The relayer binary.
//!
//! Thin over [`dray_relayer`] (ADR-006): everything of substance is in the
//! library, so the integration tests can drive a relayer in-process against
//! Anvil. Forcing a reorg and watching the response needs control over its
//! lifetime, not just its output.

use std::process::ExitCode;

use dray_relayer::{
    chain::Chain,
    config::Config,
    relayer::{Relayer, ShutdownHandle, shutdown},
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
            tracing::error!(error = %err, "relayer exited");
            eprintln!("dray-relayer: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    init_tracing(&config);

    let store = dray_store::Store::connect(&config.database_url, config.max_db_connections).await?;

    let chain = Chain::connect(&config.rpc_url, &config.private_key, config.settlement).await?;
    let relayer = Relayer::new(store, chain, config.relayer.clone());

    // Refusing to start beats producing a stream of identical failures that
    // look like job problems. An unauthorised or unfunded relayer cannot settle
    // anything, and the operator should hear that once, now.
    relayer
        .preflight()
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (handle, signal) = shutdown();
    let signals = tokio::spawn(await_signals(handle));

    let outcomes = relayer.run(signal).await;
    signals.abort();

    tracing::info!(attempts = outcomes.len(), "stopped cleanly");
    Ok(())
}

/// Translate SIGTERM and SIGINT into a shutdown request.
///
/// A relayer holding a job through a confirmation wait must be told to stop
/// rather than killed, or its lease sits until it expires while the transaction
/// it broadcast goes unwatched.
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
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => tracing::info!("interrupt received; shutting down"),
                Err(err) => tracing::error!(error = %err, "cannot listen for interrupts"),
            },
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
