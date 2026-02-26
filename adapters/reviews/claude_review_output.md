## Adversarial Review: Adapter Governance Docs vs. Engram Spec

### Blocking Findings

**1. Normalization order may conflict with chronological tape invariant**
`ADAPTERS.md:51-55` — Step 3 of the Deterministic Ingest Protocol lists a "stable order" by event type: `meta`, then `msg.*`, then `tool.*`, then `code.*`. The spec defines a Trace Tape as "an append-only chronological log of what happened during an agent session." If "stable order" means type-grouped rather than time-ordered, this reorders events and destroys temporal sequence — which is the fundamental structure of a tape. If it means "deterministic ordering within chronological sequence," the language doesn't say that.

This is ambiguous enough to cause an implementer to build the wrong thing. Needs explicit clarification: adapters must preserve chronological event order from the harness log; "stable" applies to deterministic tie-breaking within the same timestamp, not type-based reordering.

**2. Coverage kind granularity is too coarse**
`ADAPTERS.md:22` — Coverage declaration lists `msg` and `tool` as coverage kinds. The spec defines `msg.in` / `msg.out` and `tool.call` / `tool.result` as distinct event kinds. These have meaningfully different provenance characteristics — a harness log might expose `tool.call` args but not `tool.result` stdout, or might have `msg.in` (user prompts) but not `msg.out` (model responses). Declaring coverage at the `msg`/`tool` level hides this and violates the contract's own principle that "unsupported kinds must be declared as unsupported or partial, never silently claimed as complete." An adapter claiming `tool: full` when it only has `tool.call` is exactly the silent coverage inflation the contract prohibits.

Fix: coverage matrix should use the spec's six concrete event kinds (`msg.in`, `msg.out`, `tool.call`, `tool.result`, `code.read`, `code.edit`) plus `span.link` and `meta`.

---

### Non-Blocking Findings

**3. No escalation path for spec-contract drift**
`CLAUDE_REVIEWERS.md:7` — The hard rule says "review against `adapters/ADAPTERS.md` as the canonical contract." But nothing instructs reviewers (or anyone) to check whether ADAPTERS.md itself has drifted from the Engram spec. If ADAPTERS.md gets a normative error (like finding #1 above), the review process would enforce the wrong thing. Worth adding a note that spec is upstream of the adapter contract.

**4. Empty section heading**
`ADAPTERS.md:75-77` — "## Harness Versioning Policy" is a heading with no body content, immediately followed by "## Supported-Version Matrix (required)" at the same level. The matrix, version detection, and unknown-version behavior sections all look like they should be `###` children under Versioning Policy. As-is, the versioning policy section says nothing.

**5. Integration seam with `engram record` is undefined**
The spec describes `engram record --stdin` for piping in pre-existing transcripts. The adapter contract describes producing normalized JSONL. The connection between these two — whether adapters are standalone binaries piping into `record --stdin`, library code called by the CLI, or something else — is not stated anywhere. An implementer could build the right adapter contract-wise but wire it up wrong architecturally.

---

### Nits

**6.** `ADAPTERS.md:17` — "byte-equivalent normalized JSONL" — "equivalent" is slightly softer than "identical." Since this is a determinism invariant, "byte-identical" would be unambiguous.

**7.** `IMPLEMENTERS.md:28-35` — Required PR/commit notes (coverage deltas, matrix changes, etc.) are reasonable governance but heavyweight for the current project stage where zero adapters exist yet. Not harmful, just premature process.
