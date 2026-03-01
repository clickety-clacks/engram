# Engram Dispatch Marker

Status: Draft

## Problem: Multi-Party Transcript Splitting

Modern AI-assisted development rarely happens in a single conversation. A piece
of work typically crosses several distinct sessions before it touches code:

```
┌─────────────────────┐     ┌─────────────────────┐     ┌──────────────┐
│  Planning session   │     │  Coding agent        │     │  Source code │
│                     │     │                      │     │              │
│  "The scroll        │────▶│  Reads ChatView.swift│────▶│  ChatView    │
│   indicator should  │     │  Edits lines 220-240 │     │  .swift      │
│   appear after one  │     │  Runs tests          │     │  (modified)  │
│   screen of scroll" │     │                      │     │              │
└─────────────────────┘     └─────────────────────┘     └──────────────┘
       Transcript A                Transcript B
     (the reasoning)           (the implementation)
```

These sessions produce **separate transcripts** with no inherent link between
them. File-level provenance tools can trace a code change back to Transcript B
— the coding agent. But Transcript A — the conversation where the requirement
was discussed, the tradeoffs weighed, and the decision made — is invisible.

This gets worse as the chain grows:

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐     ┌──────┐
│  Product     │     │  Orchestrator│     │  Coding      │     │ Code │
│  discussion  │────▶│  planning    │────▶│  agent       │────▶│      │
│  session     │     │  session     │     │  session     │     │      │
└──────────────┘     └──────────────┘     └──────────────┘     └──────┘
  Transcript A         Transcript B         Transcript C
```

File-level evidence reaches only Transcript C. Transcripts A and B are lost.

## The Handoff Gap

The link is lost at each handoff because nothing in the downstream transcript
records *where it came from*. The coding agent's first message is the task
prompt — but nothing in that prompt ties it back to the conversation that
originated the work.

Without an explicit link, the only recovery paths are:
- **Timestamp proximity**: guess which upstream session overlaps in time
- **Vocabulary overlap**: hope that file names or function names appear in both

These are probabilistic and fail on common terms.

## Solution: Dispatch Marker

A **dispatch marker** is a UUID that the upstream party embeds in the handoff
message. Because the handoff message becomes the opening content of the
downstream session, the UUID appears verbatim in both transcripts:

```
Upstream session                    Downstream session
─────────────────                   ──────────────────
...conversation...                  [engram:src=f47ac10b-...]  ← same UUID
                                    Fix the scroll indicator...
tool_call: send_prompt(
  "[engram:src=f47ac10b-...]        ...agent work...
   Fix the scroll indicator..."
)                        ─────────▶
...                                 ...
```

No coordination with Engram is required at dispatch time. The UUID is just text.
Engram discovers the link at query time by searching all indexed transcripts for
the UUID string.

## Marker Format

```
[engram:src=<uuid>]
```

Where `<uuid>` is a UUID v4 generated fresh for each dispatch event.

The marker is prepended to the handoff message. One UUID per dispatch — not
per session. A session that was dispatched multiple times (e.g., after a
compaction/context reset) may have multiple UUIDs, each linking back to the
upstream context that re-initiated it.

**Example handoff message:**

```
[engram:src=f47ac10b-58cc-4372-a567-0e02b2c3d479]

Implement the scroll indicator feature. It should appear when the user
has scrolled up by more than one full viewport height, and disappear
with a fade when they scroll back to the bottom...
```

## Multi-Level Chains

The convention composes across arbitrary depth. Each handoff embeds a UUID;
each UUID independently links two adjacent transcripts. Engram can traverse
the full chain at query time:

```
Session A ──[uuid-1]──▶ Session B ──[uuid-2]──▶ Session C ──edits──▶ file.swift
              ↑                        ↑
       links A↔B                links B↔C
```

`engram explain file.swift:220-240` finds Session C, scans for dispatch markers,
finds `uuid-2`, retrieves Session B, scans Session B for markers, finds `uuid-1`,
retrieves Session A. The full chain surfaces without any of the intermediate
sessions needing to know about each other.

## Query Modes

### File/span query (developer-facing)

```
engram explain <file>:<start>-<end>
```

Finds coding sessions that touched the span, then traverses dispatch markers
upstream. Returns the full chain: code evidence at the bottom, orchestrator
conversations at the top.

### Dispatch query (orchestrator-facing)

```
engram explain --dispatch <uuid>
```

Entry point is the work unit, not a file location. Returns:
- The upstream session(s) containing the UUID (where the work was originated)
- The downstream session(s) containing the UUID (where the work was done)
- All code spans touched by those downstream sessions

This is the natural interface for anyone who dispatched the work and wants to
review what it produced — no file paths required.

## General Convention

The dispatch marker is not tied to any specific tool, framework, or workflow.
Any party that hands work to another party can embed `[engram:src=<uuid>]` in
the handoff. Engram will discover and surface the link without needing to know
anything about the parties involved.

Compatible with any workflow where:
- One session (human+AI chat, script, CI system, orchestrator agent) dispatches
  work to another session
- Both sessions produce transcripts that Engram can ingest

## Worked Example: Two-Tier Vibe Coding

One instantiation of this pattern: a chat session (where a human and an AI
discuss requirements) dispatches to a coding agent (which implements them).

```
Chat session (TARS)                   Coding agent (eezo)
───────────────────                   ───────────────────
Human: "The barge-in behavior         [engram:src=a3f2...]
  is broken — tapping during          Fix barge-in: tapping during
  speech should interrupt,            .speaking state must route to
  not cancel"                         bargeIn(), not stop()...
                                      
AI generates UUID a3f2...             Reads WatchMainView.swift
AI dispatches prompt with marker  ──▶ Edits line 229
                    UUID a3f2...      Runs build
  appears in this                     UUID a3f2... appears in
  session's tool call                 this session's first message
```

Later: `engram explain --dispatch a3f2...` returns both the chat discussion
and the coding session — the full picture from requirement to implementation.

## Implementation Checklist

- [ ] Marker embedding: upstream party prepends `[engram:src=<uuid>]` to
      each dispatch (e.g., in orchestrator tooling)
- [ ] Chain-explain: `engram explain` scans result sessions for marker
      pattern, traverses upstream, merges results with `tier` annotation
- [ ] `explain --dispatch <uuid>` query mode
- [ ] `tier` field on session results: `agent` | `orchestrator`
- [ ] Tests: multi-session fixture sharing UUIDs at each level
- [ ] Docs: reference in README and DESIGN.md
