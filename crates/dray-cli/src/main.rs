//! Operator CLI (`dray`).
//!
//! The full command set — replaying failures, draining workers, queue
//! statistics — belongs to Phase 5. What is here is the subset `make e2e`
//! needs, brought forward because the end-to-end script needs a way to prepare
//! a database and read a settlement back without assuming `psql` is installed.
//!
//! That is not merely convenient. The README promises a quickstart that needs
//! only Docker and Rust, and a script that quietly required a Postgres client
//! package would make that untrue for anyone who did not happen to have one.
//!
//! Arguments are parsed by hand rather than with a library. Six subcommands do
//! not justify a dependency, and the shape of the CLI is still Phase 5's to
//! settle; adding one now would be committing to it early.

use std::process::ExitCode;

use dray_store::Store;

const USAGE: &str = "\
dray — operator CLI

USAGE:
    dray <COMMAND>

COMMANDS:
    migrate                 Apply migrations to $DATABASE_URL
    reset <database>        Drop and recreate <database> on the same server
    exec <file.sql>         Run a SQL file against $DATABASE_URL
    job <id>                Print a job as JSON
    settlement <id>         Print a job's latest settlement as JSON
    queue                   Print queue depth and state counts as JSON

ENVIRONMENT:
    DATABASE_URL            Defaults to postgres://dray:dray@localhost:5432/dray
";

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
            eprintln!("dray: {err}");
            ExitCode::FAILURE
        }
    }
}

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dray:dray@localhost:5432/dray".to_owned())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(());
    };

    match command {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }

        "migrate" => {
            let store = Store::connect(&database_url(), 2).await?;
            store.migrate().await?;
            eprintln!("migrations applied");
            Ok(())
        }

        "reset" => {
            let name = args.get(1).ok_or("reset needs a database name")?;
            reset(name).await
        }

        "exec" => {
            let path = args.get(1).ok_or("exec needs a path to a .sql file")?;
            let sql = std::fs::read_to_string(path)?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&database_url())
                .await?;
            // `execute` rather than `query`: seed files and migrations are
            // multi-statement, and a prepared query accepts only one.
            sqlx::raw_sql(&sql).execute(&pool).await?;
            pool.close().await;
            eprintln!("applied {path}");
            Ok(())
        }

        "job" => {
            let id = parse_id(args.get(1))?;
            let store = Store::connect(&database_url(), 2).await?;
            let job = store.job(id).await?.ok_or("no such job")?;

            println!(
                "{}",
                serde_json::json!({
                    "id": job.id,
                    "circuit_id": job.circuit_id,
                    "state": job.state.as_str(),
                    "attempts": job.attempts,
                    "submission_attempts": job.submission_attempts,
                    "max_attempts": job.max_attempts,
                    "last_error": job.last_error,
                    "leased_by": job.leased_by,
                    "proof_size_bytes": job.proof.as_ref().map(Vec::len),
                    "retry_after": job.retry_after,
                })
            );
            Ok(())
        }

        "settlement" => {
            let id = parse_id(args.get(1))?;
            let store = Store::connect(&database_url(), 2).await?;
            let settlement = store
                .latest_settlement(id)
                .await?
                .ok_or("no settlement recorded for that job")?;

            println!(
                "{}",
                serde_json::json!({
                    "job_id": settlement.job_id,
                    "tx_hash": hex(&settlement.tx_hash),
                    "nullifier": hex(&settlement.nullifier),
                    "block_number": settlement.block_number,
                    "confirmations": settlement.confirmations,
                    "gas_used": settlement.gas_used,
                    "effective_gas_price": settlement.effective_gas_price,
                    "reorged_at": settlement.reorged_at,
                })
            );
            Ok(())
        }

        "queue" => {
            let store = Store::connect(&database_url(), 2).await?;
            let depth = store.queue_depth().await?;
            let counts: serde_json::Map<String, serde_json::Value> = store
                .state_counts()
                .await?
                .into_iter()
                .map(|(state, count)| (state.as_str().to_owned(), count.into()))
                .collect();

            println!(
                "{}",
                serde_json::json!({"queue_depth": depth, "states": counts})
            );
            Ok(())
        }

        other => {
            eprint!("unknown command {other:?}\n\n{USAGE}");
            Err("unknown command".into())
        }
    }
}

/// Drop and recreate a database on the same server as `DATABASE_URL`.
///
/// Connects to the *server* named by `DATABASE_URL` and operates on `name`, so
/// the target need not exist yet — and, more to the point, so this cannot be
/// pointed at the database it is connected to.
async fn reset(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Postgres identifiers cannot be bound as parameters, so the name is
    // checked rather than escaped. A database name is an operator-supplied
    // string reaching a statement that cannot be parameterised; restricting it
    // to a conservative character set is the honest way to keep that safe.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "{name:?} is not a usable database name: letters, digits, underscore and \
             hyphen only"
        )
        .into());
    }

    let url = database_url();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;

    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
        .execute(&pool)
        .await?;
    sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
        .execute(&pool)
        .await?;
    pool.close().await;

    eprintln!("recreated database {name}");
    Ok(())
}

fn parse_id(raw: Option<&String>) -> Result<uuid::Uuid, Box<dyn std::error::Error>> {
    let raw = raw.ok_or("this command needs a job id")?;
    Ok(raw.parse()?)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_prefixed_and_fixed_width() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "0x000fff");
        assert_eq!(hex(&[]), "0x");
    }

    #[test]
    fn a_malformed_job_id_is_reported_not_ignored() {
        assert!(parse_id(Some(&"not-a-uuid".to_owned())).is_err());
        assert!(parse_id(None).is_err());
        assert!(parse_id(Some(&uuid::Uuid::new_v4().to_string())).is_ok());
    }

    /// A database name reaches a statement that cannot be parameterised, so it
    /// is validated rather than escaped. Anything outside a conservative set is
    /// refused rather than quoted and hoped for.
    #[tokio::test]
    async fn a_hostile_database_name_is_refused() {
        for name in [
            "",
            r#"a" WITH (FORCE); DROP DATABASE "dray"#,
            "has space",
            "semi;colon",
            "back`tick",
        ] {
            let err = reset(name).await.expect_err("{name:?} should be refused");
            assert!(
                err.to_string().contains("not a usable database name"),
                "{name:?} was refused for the wrong reason: {err}"
            );
        }
    }

    #[test]
    fn the_usage_text_lists_every_command() {
        for command in ["migrate", "reset", "exec", "job", "settlement", "queue"] {
            assert!(USAGE.contains(command), "{command} missing from usage");
        }
    }
}
