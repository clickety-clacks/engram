# DESIGN.md Revision Brief

## Source: Flynn conversation 2026-02-25

## Core stance change

Engram is NOT a git-repo-scoped tool that happens to support external tapes. Engram is a **system-wide provenance index** that happens to integrate well with git repos.

## Key principles to establish

1. **Tapes are just files. Fingerprints are just hashes.** No central authority, no accounts, no servers. Point Engram at folders of tapes, it indexes them.

2. **No project taxonomy.** Engram doesn't know or care what a "project" is. A fingerprint match is a fingerprint match. Cross-project connections surface automatically because the text overlaps — no one has to wire them up.

3. **Scope is user-defined.** You declare what Engram can see via source paths. Exclude something = Engram can't see it. Trade recall for privacy. Your choice, not Engram's.

4. **Provenance is additive.** Start with zero, add repo tapes, add orchestrator tapes, add cross-project tapes. Each layer enriches. None is required. Some > none, always.

5. **Portability via files, not protocols.** Want to share provenance? Zip tapes, send them. Recipient points Engram at the folder. Index rebuilds. Done.

## Architecture (replaces current on-disk layout)

- **`.engram/` per repo** — coding agent tapes, travels with git clone. Baseline provenance.
- **`~/.engram/` (home)** — personal/orchestrator tapes, cross-project context. Private by default.
- **`.engram-cache/`** — derived index, never committed, rebuildable from any set of tapes.
- **Config** — declares sources (list of paths Engram indexes). Simple include/exclude.

```yaml
sources:
  - ~/src/engram/.engram/tapes
  - ~/src/clawline/.engram/tapes
  - ~/src/clawdbot/.engram/tapes
  - ~/.openclaw/sessions
exclude:
  - ~/.openclaw/sessions/personal-*
```

## Multi-tier provenance (new section)

In compound agent systems (orchestrator → coding agent), the causal chain splits across tiers:
- Orchestrator transcript has the WHY (decisions, reasoning, Flynn's intent)
- Coding agent transcript has the WHAT (edits, reads, tool calls)
- Both are just tapes. Engram indexes both. Fingerprints link them automatically when text overlaps (quoted code, shared context, dispatched prompts).
- No special cross-tier linking needed — if the orchestrator discussed the same code the agent edited, the fingerprints match.

In single-agent setups (pure Codex, pure Claude Code), one transcript has everything. Works the same — Engram doesn't care about the topology.

## Sections to revise in DESIGN.md

1. **Intro / What Engram Is** — reframe as system-wide, not repo-scoped
2. **Design Principles** — weaken/remove P7 (Git coexistence) as a core principle. Git integration becomes ONE way to use Engram, not THE way. Keep P8 (local-first).
3. **On-Disk Layout** — show the multi-source model (per-repo + home + config)
4. **Repository Hygiene** — rename to "Git Integration" and make it one section among peers, not the assumed deployment model
5. **NEW: Multi-Tier Provenance** — explain orchestrator + agent taping, how fingerprints link them
6. **NEW: Scope and Privacy** — user-defined boundaries, recall vs privacy tradeoff
7. **NEW: Sharing Provenance** — zip and send, additive layering, no protocol needed
8. **Adapter Model** — add OpenClaw/orchestrator adapter alongside coding agent adapters
9. **Enrichment Model** — unchanged (still additive/optional)

## What stays the same

- Core fingerprinting mechanism — unchanged
- Tape format — unchanged  
- Evidence index — unchanged
- Query algorithm — unchanged
- CLI commands — unchanged (but `engram init` might create config pointing at sources)
- Span linkage / tombstones — unchanged
- Saliency layer separation — unchanged
- Continuation detection — unchanged (timestamps + overlap, works across all tape sources)

## Flynn quotes (for tone)

- "Does Engram even care about what's a project and what's not?"
- "By definition someone in that project was talking about it — maybe that's important context"
- "We can make it really easy to define the edges of what Engram sees, knowing full well that some provenance could disappear, but that's just the price we pay for privacy"
- "It's just all fingerprints, right?"
