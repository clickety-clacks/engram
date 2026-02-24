# Engram Adapters

Purpose: define the contract for adapters that convert harness logs into Engram tape events.

This is the canonical adapter contract for this repo.

---

## What an adapter is

An adapter is a deterministic translator:

`harness artifacts -> normalized Engram JSONL events`

Adapters are not summarizers and not analyzers. They preserve facts.

---

## Compliance levels

### Required (adapter is non-viable without these)

1. **Deterministic output**
   - Same input bytes -> same output bytes.
   - No LLM-based interpretation.

2. **Truthful coverage declaration**
   - Adapter must declare coverage for each event kind:
     - `meta`, `msg.in`, `msg.out`, `tool.call`, `tool.result`, `code.read`, `code.edit`, `span.link`
   - Coverage values are only: `full`, `partial`, `none`.
   - No silent `full` claims for partial extraction.

3. **Version-aware behavior**
   - Adapter must identify harness version/schema (or mark unknown deterministically).
   - Unknown versions must either:
     - fail (`strict` mode), or
     - ingest safe subset with explicit degraded coverage (`permissive` mode).

4. **Machine-readable failure**
   - Parse/version failures must return explicit machine-readable errors.

5. **Raw fact preservation**
   - Preserve key source facts used for auditability (`tool`, args/input, outputs, IDs, timestamps).

### Valuable but optional (improves quality, not disqualifying)

- Better call/result correlation diagnostics
- Large-output artifact dereferencing
- Extra deterministic projections (e.g., patch-file lists)

---

## Adapter interface (required)

### Rust interface (library)

```rust
pub trait HarnessAdapter {
    fn id(&self) -> AdapterId;

    /// Converts harness artifacts into Engram JSONL events.
    /// Must be deterministic.
    fn convert_to_tape_jsonl(&self, input: &str) -> Result<String, AdapterError>;

    /// Declares supported versions and coverage guarantees.
    fn descriptor(&self) -> AdapterDescriptor;
}
```

### Descriptor contract

Each adapter descriptor must include:

- `adapter_id`
- `adapter_version`
- `harness_family`
- `supported_versions` (min/max-tested or explicit ranges)
- `schema_detection_strategy`
- per-kind coverage map (`full|partial|none`)
- unknown-version policy (`strict_fail` or `permissive_degrade`)

### Ingest runtime contract

The ingest path must be able to:

1. detect harness family/version,
2. pick adapter,
3. normalize to Engram events,
4. attach adapter metadata,
5. enforce strict/permissive policy.

---

## Event output contract

Normalized output must only contain Engram event kinds:

- `meta`
- `msg.in`
- `msg.out`
- `tool.call`
- `tool.result`
- `code.read`
- `code.edit`
- `span.link`

Ordering rules:

- Preserve source chronology.
- If timestamps tie, preserve stable source order.
- Never reorder by event kind.

---

## Versioning policy

Each adapter must maintain a version matrix with:

- harness family
- supported versions/ranges
- known-bad versions (if any)
- guaranteed coverage profile per range

Unknown schema/version handling:

- **strict mode**: fail with `unknown_harness_version`
- **permissive mode**: ingest deterministic safe subset + emit degraded coverage

---

## CI gates (must pass)

1. Fixture conformance tests (version-pinned)
2. Determinism test (repeat parse byte-identical)
3. Coverage-claim tests (`full/partial/none` must match fixtures)
4. Unknown-version behavior tests (strict fail + permissive degrade)
5. Contract sync test (descriptor/docs/code consistency)

No adapter release if any gate fails.

---

## Current adapter expectations

- **Claude Code**: should target high deterministic coverage.
- **Codex CLI**: must be explicit where read/edit remain partial.
- **OpenCode / Gemini / Cursor**: may be partial while discovery matures, but coverage must be explicit and truthful.

---

## Reviewer checklist

Reject changes that:

- add unspecced inference,
- silently degrade behavior,
- claim `full` without fixture proof,
- guess version/schema non-deterministically.

Prefer correctness of boundaries over breadth of claimed support.
