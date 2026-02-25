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

1. **Emit parseable text content**
   - At minimum, an adapter must emit the raw transcript content deterministically.
   - Engram fingerprints all content uniformly. Classification into event kinds is not required for basic provenance to work.
   - Same input bytes -> same output bytes. No LLM-based interpretation.

2. **Version-aware behavior**
   - Adapter must identify harness version/schema (or mark unknown deterministically).
   - Unknown versions must either:
     - fail (`strict` mode), or
     - ingest safe subset with explicit degraded coverage (`permissive` mode).

3. **Machine-readable failure**
   - Parse/version failures must return explicit machine-readable errors.

### Enrichment tier (improves quality, not disqualifying if absent)

1. **Event kind classification** (`msg.in`, `msg.out`, `tool.call`, `tool.result`, `code.read`, `code.edit`, `span.link`, `meta`)
   - Enables confidence tiers, structured traversal, and richer query output.
   - Coverage declaration per kind: `full`, `partial`, `none`.

2. **Structured edit/read spans** (file paths, ranges, before/after hashes)
   - Enables tombstones, high-confidence anchors, and lineage edges.

3. **Call/result correlation, artifact dereferencing, patch-file lists**
   - Further enrichment for diagnostics and display.

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

| Kind | What it captures | Key fields |
|------|-----------------|------------|
| `meta` | Session metadata: model, repo state, harness identity | `model`, `repo_head`, `label`, `source` |
| `msg.in` | User/human prompt or task input to the agent | `text` |
| `msg.out` | Agent/assistant response or reasoning output | `text` |
| `tool.call` | Agent invokes a tool (read file, run command, edit, etc.) | `tool`, `call_id`, `args` |
| `tool.result` | Result returned from a tool invocation | `tool`, `call_id`, `stdout`, `stderr`, `exit` |
| `code.read` | Agent reads a specific file/range | `file`, `range`, `anchor_hashes` |
| `code.edit` | Agent modifies a specific file/range | `file`, `before_hash`, `after_hash`, `before_range`, `after_range` |
| `span.link` | Explicit agent-declared lineage between two spans | `from_file`, `from_range`, `to_file`, `to_range`, `note` |

All events also carry: `t` (ISO timestamp), `k` (event kind), `source` (harness + session ID).

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
