//! Turning validated job inputs into a proof.
//!
//! Two subprocesses, both bounded (see [`crate::bounded`]):
//!
//! 1. `nargo execute` solves the witness from `Prover.toml`.
//! 2. `bb prove -t evm` produces the proof and the public input vector.
//!
//! # Every job gets its own copy of the circuit
//!
//! `nargo` is directory-oriented: it reads `Prover.toml` from the package
//! directory and writes the witness into that package's `target/`. Both are
//! fixed paths. Two workers proving the same circuit at once against a shared
//! package would therefore overwrite each other's inputs and each other's
//! witnesses — and the damage would not be a crash but a *wrong proof*, since
//! the second worker would happily prove the first worker's witness and record
//! it against its own job.
//!
//! So each job gets a private copy of the package inside its scratch
//! directory. Measured at 0.33 s and ~120 KB per job against roughly 2.5 s of
//! proving, which is a cheap price for removing a whole class of concurrency
//! bug rather than trying to lock around it.
//!
//! It also keeps client secrets out of the repository. `Prover.toml` contains
//! the private inputs; writing it into a checked-out working tree would leave
//! them there for the next person to find.
//!
//! # What is prepared once
//!
//! Compilation output and the verification key are identical for every job of a
//! circuit, and the key costs about two seconds to produce. [`Artifacts`]
//! prepares both once at startup; only witness generation and proving are
//! per-job.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::bounded::{self, BoundedError, Bounds, Scratch};

/// Name of the prover input file `nargo` reads. Not configurable in the
/// worker: the copied package is private to one job, so there is nothing to
/// disambiguate.
const PROVER_TOML: &str = "Prover.toml";

/// Why proving a job did not produce a proof.
#[derive(Debug, thiserror::Error)]
pub enum ProveError {
    #[error("no prepared artefacts for circuit {0}")]
    UnknownCircuit(String),

    #[error("inputs must be a JSON object, found {found}")]
    InputsNotAnObject { found: &'static str },

    #[error("input {field} cannot be written to Prover.toml: {reason}")]
    UnrepresentableInput { field: String, reason: String },

    #[error("preparing the scratch directory failed: {0}")]
    Scratch(String),

    #[error("witness generation failed: {0}")]
    Witness(#[source] BoundedError),

    #[error("proof generation failed: {0}")]
    Proving(#[source] BoundedError),

    #[error("{artefact} was not produced")]
    MissingArtefact { artefact: &'static str },
}

impl ProveError {
    /// How the store should record this failure.
    ///
    /// The distinction is what stops the queue wasting the most expensive
    /// resource it has on work that cannot succeed. Inputs that cannot be
    /// written, or that `nargo` rejects, will be rejected identically on every
    /// retry; resource exhaustion may well not be.
    #[must_use]
    pub fn kind(&self) -> dray_core::FailureKind {
        use dray_core::FailureKind::{Permanent, Transient};
        match self {
            // A circuit the worker has no artefacts for is an operator error,
            // not a property of the job — a worker started with a stale
            // artefact directory would otherwise permanently fail every job
            // for a circuit that was only added recently. Another worker may
            // have it.
            ProveError::UnknownCircuit(_) => Transient,

            ProveError::InputsNotAnObject { .. } | ProveError::UnrepresentableInput { .. } => {
                Permanent
            }

            ProveError::Scratch(_) => Transient,

            // Delegate: a bound exceeded is transient, a tool that rejected its
            // input is not.
            ProveError::Witness(err) | ProveError::Proving(err) => {
                if err.is_transient() {
                    Transient
                } else {
                    Permanent
                }
            }

            // The tool exited zero without writing what it promised. Nothing
            // about the job explains that, so let it be retried.
            ProveError::MissingArtefact { .. } => Transient,
        }
    }

    /// The label used for this failure in metrics.
    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            ProveError::UnknownCircuit(_) => "unknown_circuit",
            ProveError::InputsNotAnObject { .. } | ProveError::UnrepresentableInput { .. } => {
                "bad_inputs"
            }
            ProveError::Scratch(_) => "scratch_failed",
            ProveError::Witness(err) | ProveError::Proving(err) => err.metric_label(),
            ProveError::MissingArtefact { .. } => "missing_artefact",
        }
    }
}

/// A finished proof and what it cost.
#[derive(Debug, Clone)]
pub struct Proven {
    pub proof: Vec<u8>,
    /// The public input vector, 32 bytes per field element. The nullifier is
    /// the last element (ADR-008).
    pub public_inputs: Vec<u8>,
    pub duration: Duration,
    pub peak_memory_kb: Option<u64>,
}

impl Proven {
    /// The nullifier this proof consumes on settlement.
    ///
    /// Returns `None` if the vector is empty or not a whole number of field
    /// elements, rather than panicking on a slice — a malformed vector from a
    /// tool that exited zero is a bug worth surfacing as a failed job, not one
    /// worth taking the worker down for.
    #[must_use]
    pub fn nullifier(&self) -> Option<[u8; 32]> {
        // `% 32` rather than `is_multiple_of`, which is newer than this
        // workspace's MSRV.
        if self.public_inputs.is_empty() || self.public_inputs.len() % 32 != 0 {
            return None;
        }
        let start = self.public_inputs.len() - 32;
        self.public_inputs[start..].try_into().ok()
    }
}

/// Per-circuit artefacts prepared once and shared by every job.
///
/// `root` is expected to contain one directory per circuit, each holding a
/// `package/` (the Noir sources plus the compiled `target/`) and a `vk`.
/// [`Artifacts::prepare`] builds that layout from a circuits workspace.
#[derive(Debug, Clone)]
pub struct Artifacts {
    root: PathBuf,
}

impl Artifacts {
    /// Adopt an already-prepared artefact directory.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn package_dir(&self, circuit: &str) -> PathBuf {
        self.root.join(circuit).join("package")
    }

    fn vk_path(&self, circuit: &str) -> PathBuf {
        self.root.join(circuit).join("vk")
    }

    /// Whether this circuit has usable artefacts.
    #[must_use]
    pub fn has(&self, circuit: &str) -> bool {
        self.package_dir(circuit).join("Nargo.toml").is_file() && self.vk_path(circuit).is_file()
    }
}

/// Everything the prover needs that is not the job itself.
#[derive(Debug, Clone)]
pub struct ProverConfig {
    pub artifacts: Artifacts,
    /// Parent directory for per-job scratch directories.
    pub scratch_root: PathBuf,
    pub bounds: Bounds,
    /// Program names, so tests can substitute stubs and operators can point at
    /// a pinned install without relying on `PATH`.
    pub nargo: String,
    pub bb: String,
}

impl ProverConfig {
    #[must_use]
    pub fn new(artifacts: Artifacts, scratch_root: impl Into<PathBuf>) -> Self {
        Self {
            artifacts,
            scratch_root: scratch_root.into(),
            bounds: Bounds::for_proving(),
            nargo: "nargo".to_owned(),
            bb: "bb".to_owned(),
        }
    }
}

/// Prove one job.
///
/// # Errors
///
/// Returns [`ProveError`]; call [`ProveError::kind`] to decide whether the job
/// should be retried.
pub async fn prove(
    circuit_id: &str,
    inputs: &serde_json::Value,
    label: &str,
    config: &ProverConfig,
) -> Result<Proven, ProveError> {
    if !config.artifacts.has(circuit_id) {
        return Err(ProveError::UnknownCircuit(circuit_id.to_owned()));
    }

    // Rendered before any directory is created: a job with unrepresentable
    // inputs fails permanently, so there is no reason to pay for setup first.
    let prover_toml = render_prover_toml(inputs)?;

    // Absolute for the same reason as in `prepare`: the paths below are handed
    // to subprocesses whose working directory is not this one.
    let scratch_root = absolute(&config.scratch_root)?;
    let scratch = Scratch::new(&scratch_root, label)
        .map_err(|e| ProveError::Scratch(format!("creating scratch for {label}: {e}")))?;

    let package = scratch.path().join("package");
    copy_dir(&config.artifacts.package_dir(circuit_id), &package)
        .map_err(|e| ProveError::Scratch(format!("copying the circuit package: {e}")))?;

    std::fs::write(package.join(PROVER_TOML), &prover_toml)
        .map_err(|e| ProveError::Scratch(format!("writing {PROVER_TOML}: {e}")))?;

    // ---- witness ----------------------------------------------------------
    bounded::run(
        &config.nargo,
        &["execute".to_owned()],
        &package,
        config.bounds,
    )
    .await
    .map_err(ProveError::Witness)?;

    let bytecode =
        find_one(&package.join("target"), "json").ok_or(ProveError::MissingArtefact {
            artefact: "compiled circuit",
        })?;
    let witness = find_one(&package.join("target"), "gz").ok_or(ProveError::MissingArtefact {
        artefact: "witness",
    })?;

    // ---- proof ------------------------------------------------------------
    let out = scratch.path().join("out");
    std::fs::create_dir_all(&out)
        .map_err(|e| ProveError::Scratch(format!("creating the output directory: {e}")))?;

    // `-t evm` selects the keccak transcript the generated Solidity verifier
    // expects. The key, the proof, and the verifier must all agree on it, and
    // the key was written with the same flag in `prepare`.
    let args = [
        "prove".to_owned(),
        "-t".to_owned(),
        "evm".to_owned(),
        "-b".to_owned(),
        path_arg(&bytecode)?,
        "-w".to_owned(),
        path_arg(&witness)?,
        "-k".to_owned(),
        path_arg(&config.artifacts.vk_path(circuit_id))?,
        "-o".to_owned(),
        path_arg(&out)?,
    ];
    let completed = bounded::run(&config.bb, &args, scratch.path(), config.bounds)
        .await
        .map_err(ProveError::Proving)?;

    let proof = std::fs::read(out.join("proof"))
        .map_err(|_| ProveError::MissingArtefact { artefact: "proof" })?;
    let public_inputs =
        std::fs::read(out.join("public_inputs")).map_err(|_| ProveError::MissingArtefact {
            artefact: "public_inputs",
        })?;

    // `scratch` drops here, taking the witness, the copied package, and the
    // client's private inputs with it.
    Ok(Proven {
        proof,
        public_inputs,
        duration: completed.duration,
        peak_memory_kb: completed.peak_memory_kb,
    })
}

/// Build the artefact directory for `circuits` from a Noir workspace.
///
/// Compiles the workspace, then writes each circuit's verification key. Run
/// once at worker startup.
///
/// # Errors
///
/// Returns [`ProveError`] if compilation, key generation, or copying fails.
pub async fn prepare(
    circuits_dir: &Path,
    circuits: &[String],
    into: &Path,
    config: &ProverConfig,
) -> Result<Artifacts, ProveError> {
    let bounds = Bounds::for_preparation();

    // Every path handed to a subprocess is made absolute first. These commands
    // run with their working directory set elsewhere — `nargo` in the circuits
    // workspace, `bb` in the artefact directory — so a relative path resolves
    // against the wrong place and the tool reports a missing file. The tests
    // canonicalise their fixtures and so never saw it; the binary's default of
    // `circuits` is relative, and did.
    let circuits_dir = &absolute(circuits_dir)?;
    let into = &absolute(into)?;

    bounded::run(&config.nargo, &["compile".to_owned()], circuits_dir, bounds)
        .await
        .map_err(ProveError::Witness)?;

    for circuit in circuits {
        let dest = into.join(circuit);
        let package = dest.join("package");
        std::fs::create_dir_all(&package)
            .map_err(|e| ProveError::Scratch(format!("creating {}: {e}", package.display())))?;

        copy_dir(&circuits_dir.join(circuit), &package)
            .map_err(|e| ProveError::Scratch(format!("copying {circuit}: {e}")))?;

        // The workspace writes compiled output to a shared target directory;
        // give the copied package its own so `nargo execute` finds it without
        // recompiling.
        let target = package.join("target");
        std::fs::create_dir_all(&target)
            .map_err(|e| ProveError::Scratch(format!("creating {}: {e}", target.display())))?;
        let compiled = circuits_dir.join("target").join(format!("{circuit}.json"));
        std::fs::copy(&compiled, target.join(format!("{circuit}.json")))
            .map_err(|e| ProveError::Scratch(format!("copying {}: {e}", compiled.display())))?;

        // A stale Prover.toml carried over from the repository would be used by
        // any job whose own inputs failed to write. Remove it so that cannot
        // happen quietly.
        let _ = std::fs::remove_file(package.join(PROVER_TOML));

        let args = [
            "write_vk".to_owned(),
            "-t".to_owned(),
            "evm".to_owned(),
            "-b".to_owned(),
            path_arg(&compiled)?,
            "-o".to_owned(),
            path_arg(&dest)?,
        ];
        bounded::run(&config.bb, &args, &dest, bounds)
            .await
            .map_err(ProveError::Proving)?;
    }

    Ok(Artifacts::at(into))
}

// ---------------------------------------------------------------------------
// Prover.toml
// ---------------------------------------------------------------------------

/// Render validated job inputs as a `Prover.toml`.
///
/// Deliberately narrow. Noir accepts field elements as decimal or hexadecimal
/// *strings*, integers, and booleans, plus arrays and structs of those. This
/// writes exactly that and refuses everything else, because the alternative to
/// refusing is emitting something `nargo` will reject after the worker has
/// already leased the job and created a directory for it.
///
/// Floats are rejected for the same reason they are rejected in the canonical
/// job hash: there is no lossless mapping from a JSON float to a field element,
/// and silently truncating one would change the statement being proved.
fn render_prover_toml(inputs: &serde_json::Value) -> Result<String, ProveError> {
    let object = inputs.as_object().ok_or(ProveError::InputsNotAnObject {
        found: type_name(inputs),
    })?;

    // TOML binds a bare key to the table it was most recently opened under, so
    // every scalar and array has to be emitted before the first `[table]`
    // header. Sorting by shape rather than trusting input order is what makes
    // that safe.
    let mut scalars = String::new();
    let mut tables = String::new();

    for (key, value) in object {
        if let serde_json::Value::Object(nested) = value {
            tables.push_str(&format!("\n[{key}]\n"));
            for (inner_key, inner) in nested {
                let rendered = render_value(inner, &format!("{key}.{inner_key}"))?;
                tables.push_str(&format!("{inner_key} = {rendered}\n"));
            }
        } else {
            let rendered = render_value(value, key)?;
            scalars.push_str(&format!("{key} = {rendered}\n"));
        }
    }

    Ok(format!("{scalars}{tables}"))
}

fn render_value(value: &serde_json::Value, field: &str) -> Result<String, ProveError> {
    use serde_json::Value;

    match value {
        Value::String(s) => Ok(quote_toml(s)),
        Value::Bool(b) => Ok(b.to_string()),

        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Ok(u.to_string())
            } else {
                // A float, or an integer too large for 64 bits. Field elements
                // routinely exceed 64 bits, which is exactly why they belong in
                // the schemas as strings.
                Err(ProveError::UnrepresentableInput {
                    field: field.to_owned(),
                    reason: format!(
                        "{n} is not an integer representable in 64 bits; \
                         field elements must be given as decimal or hex strings"
                    ),
                })
            }
        }

        Value::Array(items) => {
            let rendered: Result<Vec<String>, ProveError> = items
                .iter()
                .enumerate()
                .map(|(i, item)| render_value(item, &format!("{field}[{i}]")))
                .collect();
            Ok(format!("[{}]", rendered?.join(", ")))
        }

        Value::Null => Err(ProveError::UnrepresentableInput {
            field: field.to_owned(),
            reason: "null has no field element representation".to_owned(),
        }),

        Value::Object(_) => Err(ProveError::UnrepresentableInput {
            field: field.to_owned(),
            reason: "nested objects are only supported at the top level".to_owned(),
        }),
    }
}

/// Quote a string as a TOML basic string.
///
/// Job inputs are client-supplied. A value containing a quote or a newline must
/// not be able to end the string early and have the remainder parsed as TOML —
/// that would let a caller add or replace inputs to the circuit.
fn quote_toml(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // TOML forbids raw control characters in basic strings.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Resolve a path to an absolute one, creating it if it does not yet exist.
///
/// `canonicalize` requires the path to exist, and an artefact directory
/// legitimately may not on a first run — so it is created rather than treated
/// as an error.
fn absolute(path: &Path) -> Result<PathBuf, ProveError> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .map_err(|e| ProveError::Scratch(format!("creating {}: {e}", path.display())))?;
    }

    path.canonicalize()
        .map_err(|e| ProveError::Scratch(format!("resolving {}: {e}", path.display())))
}

/// Reject paths that are not valid UTF-8 rather than lossily converting them.
///
/// The argument is about to be quoted into a shell command. A lossy conversion
/// would produce a path that does not name the file that was meant, and the
/// resulting error would point at the wrong thing entirely.
fn path_arg(path: &Path) -> Result<String, ProveError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ProveError::Scratch(format!("path is not valid UTF-8: {}", path.display())))
}

/// The single file in `dir` with the given extension, if there is exactly one.
///
/// Exactly one, not the first: two witnesses in a scratch directory means
/// something is wrong with the copy, and proving an arbitrary one of them would
/// record a proof against the wrong job.
fn find_one(dir: &Path, extension: &str) -> Option<PathBuf> {
    let mut found = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().is_some_and(|e| e == extension) {
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }
    found
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&source, &destination)?;
        } else {
            std::fs::copy(&source, &destination)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(value: serde_json::Value) -> String {
        render_prover_toml(&value).expect("should render")
    }

    #[test]
    fn renders_the_membership_witness() {
        let toml = render(json!({
            "root": "0x0891",
            "secret": "42",
            "leaf_index": "5",
            "siblings": ["7", "7", "7"],
        }));

        assert!(toml.contains(r#"root = "0x0891""#), "{toml}");
        assert!(toml.contains(r#"secret = "42""#), "{toml}");
        assert!(
            toml.contains(r#"siblings = ["7", "7", "7"]"#),
            "arrays render inline: {toml}"
        );
    }

    #[test]
    fn renders_integers_and_booleans_unquoted() {
        let toml = render(json!({"min": 18, "flag": true}));
        assert!(toml.contains("min = 18"), "{toml}");
        assert!(toml.contains("flag = true"), "{toml}");
    }

    /// Field elements exceed 64 bits, so a schema that lets one through as a
    /// JSON number would silently lose precision. Rejecting is the only honest
    /// option; the schemas ask for strings.
    #[test]
    fn rejects_a_number_too_large_to_represent() {
        let huge: serde_json::Value =
            serde_json::from_str(r#"{"secret": 21888242871839275222246405745257275088}"#)
                .expect("valid json");
        let err = render_prover_toml(&huge).expect_err("should reject");
        assert!(matches!(err, ProveError::UnrepresentableInput { .. }));
        assert_eq!(err.kind(), dray_core::FailureKind::Permanent);
    }

    #[test]
    fn rejects_floats() {
        let err = render_prover_toml(&json!({"value": 1.5})).expect_err("should reject");
        assert!(err.to_string().contains("value"), "{err}");
        assert_eq!(err.kind(), dray_core::FailureKind::Permanent);
    }

    #[test]
    fn rejects_null() {
        let err = render_prover_toml(&json!({"secret": null})).expect_err("should reject");
        assert!(err.to_string().contains("null"), "{err}");
    }

    #[test]
    fn rejects_inputs_that_are_not_an_object() {
        let err = render_prover_toml(&json!([1, 2, 3])).expect_err("should reject");
        assert!(err.to_string().contains("array"), "{err}");
    }

    #[test]
    fn reports_the_offending_element_of_an_array() {
        let err =
            render_prover_toml(&json!({"siblings": ["7", 1.5, "7"]})).expect_err("should reject");
        assert!(
            err.to_string().contains("siblings[1]"),
            "should name the element: {err}"
        );
    }

    /// A client-supplied string must not be able to close its own quote and
    /// have the rest parsed as TOML — that would let it add or overwrite inputs
    /// to the circuit.
    #[test]
    fn a_quote_in_a_value_cannot_inject_another_key() {
        let toml = render(json!({"secret": "1\"\nleaf_index = \"999"}));

        let injected = toml
            .lines()
            .filter(|line| line.trim_start().starts_with("leaf_index"))
            .count();
        assert_eq!(injected, 0, "injected a key: {toml}");
        assert_eq!(toml.lines().count(), 1, "value escaped its line: {toml}");
        assert!(toml.contains(r#"\""#), "quote was not escaped: {toml}");
        assert!(toml.contains(r"\n"), "newline was not escaped: {toml}");
    }

    #[test]
    fn a_backslash_in_a_value_is_escaped() {
        let toml = render(json!({"secret": r"back\slash"}));
        assert!(toml.contains(r"back\\slash"), "{toml}");
    }

    /// TOML forbids raw control characters in a basic string, so a NUL or a
    /// bell smuggled through a permissive schema must come out as an escape
    /// rather than as a file `nargo` refuses to parse.
    #[test]
    fn control_characters_are_escaped() {
        let toml = render(json!({"secret": "a\u{0}b\u{7}"}));

        assert!(toml.contains("\\u0000"), "NUL not escaped: {toml:?}");
        assert!(toml.contains("\\u0007"), "bell not escaped: {toml:?}");
        assert!(
            !toml.chars().any(|c| (c as u32) < 0x20 && c != '\n'),
            "a raw control character survived: {toml:?}"
        );
    }

    /// TOML binds bare keys to the most recently opened table, so a scalar
    /// emitted after a `[table]` header would silently land inside it.
    #[test]
    fn scalars_are_emitted_before_any_table() {
        // "a_table" sorts before "z_scalar", so a naive in-order renderer
        // would put the scalar inside the table.
        let toml = render(json!({
            "a_table": {"inner": "1"},
            "z_scalar": "2",
        }));

        let table_at = toml.find("[a_table]").expect("table header");
        let scalar_at = toml.find("z_scalar").expect("scalar");
        assert!(
            scalar_at < table_at,
            "scalar landed inside the table:\n{toml}"
        );
    }

    #[test]
    fn nested_objects_render_as_tables() {
        let toml = render(json!({"point": {"x": "1", "y": "2"}}));
        assert!(toml.contains("[point]"), "{toml}");
        assert!(toml.contains(r#"x = "1""#), "{toml}");
    }

    #[test]
    fn doubly_nested_objects_are_rejected_rather_than_mangled() {
        let err = render_prover_toml(&json!({"outer": {"inner": {"deeper": "1"}}}))
            .expect_err("should reject");
        assert!(err.to_string().contains("outer.inner"), "{err}");
    }

    #[test]
    fn empty_inputs_render_to_an_empty_file() {
        assert_eq!(render(json!({})), "");
    }

    // ---- nullifier extraction ---------------------------------------------

    fn proven(public_inputs: Vec<u8>) -> Proven {
        Proven {
            proof: vec![],
            public_inputs,
            duration: Duration::from_secs(1),
            peak_memory_kb: None,
        }
    }

    #[test]
    fn the_nullifier_is_the_last_public_input() {
        // Two field elements: a root then a nullifier, as membership publishes.
        let mut inputs = vec![0xAA; 32];
        inputs.extend_from_slice(&[0xBB; 32]);

        assert_eq!(proven(inputs).nullifier(), Some([0xBB; 32]));
    }

    #[test]
    fn the_nullifier_is_found_whatever_the_public_input_count() {
        // Three elements, as range_proof publishes. A fixed index would break.
        let mut inputs = vec![0x11; 32];
        inputs.extend_from_slice(&[0x22; 32]);
        inputs.extend_from_slice(&[0x33; 32]);

        assert_eq!(proven(inputs).nullifier(), Some([0x33; 32]));
    }

    #[test]
    fn a_malformed_public_input_vector_yields_no_nullifier() {
        assert_eq!(proven(vec![]).nullifier(), None);
        assert_eq!(proven(vec![0; 31]).nullifier(), None, "not a whole element");
        assert_eq!(proven(vec![0; 40]).nullifier(), None, "ragged tail");
    }

    // ---- failure classification -------------------------------------------

    #[test]
    fn a_bound_exceeded_is_transient_but_a_rejected_witness_is_not() {
        use dray_core::FailureKind::{Permanent, Transient};

        let timeout = ProveError::Witness(BoundedError::WallClockExceeded {
            limit: Duration::from_secs(1),
        });
        assert_eq!(timeout.kind(), Transient);
        assert_eq!(timeout.metric_label(), "timeout");

        let oom = ProveError::Proving(BoundedError::AddressSpaceExceeded { limit_kb: 1024 });
        assert_eq!(oom.kind(), Transient);
        assert_eq!(oom.metric_label(), "oom");

        // nargo rejecting the witness means the inputs do not satisfy the
        // circuit. Retrying cannot change that.
        let rejected = ProveError::Witness(BoundedError::Failed {
            code: 1,
            stderr: "Cannot satisfy constraint".to_owned(),
        });
        assert_eq!(rejected.kind(), Permanent);
    }

    /// A worker whose artefact directory predates a newly registered circuit
    /// must not permanently fail every job for it — a freshly deployed worker
    /// may well have it.
    #[test]
    fn an_unknown_circuit_is_transient() {
        let err = ProveError::UnknownCircuit("brand_new".to_owned());
        assert_eq!(err.kind(), dray_core::FailureKind::Transient);
    }

    // ---- artefact discovery ------------------------------------------------

    #[test]
    fn artifacts_report_missing_circuits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let artifacts = Artifacts::at(dir.path());
        assert!(!artifacts.has("membership"));
    }

    #[test]
    fn find_one_refuses_an_ambiguous_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.gz"), b"x").expect("write");
        assert!(find_one(dir.path(), "gz").is_some());

        std::fs::write(dir.path().join("b.gz"), b"x").expect("write");
        assert!(
            find_one(dir.path(), "gz").is_none(),
            "two candidates must not silently resolve to one"
        );
    }

    #[test]
    fn copy_dir_copies_nested_contents() {
        let source = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(source.path().join("src")).expect("mkdir");
        std::fs::write(source.path().join("Nargo.toml"), b"[package]").expect("write");
        std::fs::write(source.path().join("src/main.nr"), b"fn main() {}").expect("write");

        let destination = tempfile::tempdir().expect("tempdir");
        let into = destination.path().join("package");
        copy_dir(source.path(), &into).expect("copy");

        assert!(into.join("Nargo.toml").is_file());
        assert!(into.join("src/main.nr").is_file());
    }
}
