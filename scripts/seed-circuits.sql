-- Registers the two Phase 1 circuits with the input schemas the API validates
-- against.
--
-- These schemas are the API's only knowledge of what a circuit accepts
-- (ADR-007), which is what keeps ingest circuit-agnostic. They must match the
-- circuits' actual signatures in circuits/*/src/main.nr — nothing checks that
-- automatically yet, and that gap is recorded in PROGRESS.md.
--
--   psql "$DATABASE_URL" -f scripts/seed-circuits.sql

INSERT INTO circuits (id, display_name, input_schema, enabled) VALUES
(
    'membership',
    'Merkle membership',
    '{
        "type": "object",
        "required": ["secret", "leaf_index", "siblings"],
        "additionalProperties": false,
        "properties": {
            "secret": {
                "type": "string",
                "description": "Private value whose commitment is the leaf.",
                "pattern": "^(0x[0-9a-fA-F]+|[0-9]+)$"
            },
            "leaf_index": {
                "type": "string",
                "description": "Position of the leaf, 0 .. 2^20 - 1.",
                "pattern": "^[0-9]+$"
            },
            "siblings": {
                "type": "array",
                "description": "Authentication path, leaf upwards. Exactly TREE_DEPTH entries.",
                "items": {"type": "string", "pattern": "^(0x[0-9a-fA-F]+|[0-9]+)$"},
                "minItems": 20,
                "maxItems": 20
            }
        }
    }'::jsonb,
    TRUE
),
(
    'range_proof',
    'Range proof',
    '{
        "type": "object",
        "required": ["value", "secret", "min", "max"],
        "additionalProperties": false,
        "properties": {
            "value":  {"type": "string", "description": "Private value being proved in range.", "pattern": "^[0-9]+$"},
            "secret": {"type": "string", "description": "Binds the nullifier to this prover.", "pattern": "^(0x[0-9a-fA-F]+|[0-9]+)$"},
            "min":    {"type": "string", "description": "Inclusive lower bound, public.", "pattern": "^[0-9]+$"},
            "max":    {"type": "string", "description": "Inclusive upper bound, public.", "pattern": "^[0-9]+$"}
        }
    }'::jsonb,
    TRUE
)
ON CONFLICT (id) DO UPDATE SET
    display_name = EXCLUDED.display_name,
    input_schema = EXCLUDED.input_schema,
    enabled      = EXCLUDED.enabled;
