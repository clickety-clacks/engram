# Engram Dispatch Marker

Status: Approved

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

The link is lost at each handoff because nothing in the receiving transcript
records *where it came from*. The coding agent's first message is the task
prompt — but nothing in that prompt ties it back to the conversation that
originated the work.

Without an explicit link, the only recovery paths are:
- **Timestamp proximity**: guess which session overlaps in time
- **Vocabulary overlap**: hope that file names or function names appear in both

These are probabilistic and fail on common terms.

## Solution: Dispatch Marker

A **dispatch marker** is a UUID that the sending party embeds in the handoff
message. Because the handoff message becomes the opening content of the
receiving session, the UUID appears verbatim in both transcripts:

```
Sending session                     Receiving session
───────────────                     ─────────────────
...conversation...                  <engram-src id="f47ac10b-..."/>  ← same UUID
                                    Fix the scroll indicator...
tool_call: send_prompt(
  '<engram-src id="f47ac10b-..."/>'
   Fix the scroll indicator..."
)                        ─────────▶
...                                 ...
```

No coordination with Engram is required at dispatch time. The UUID is just text.
Engram discovers the link at query time by searching all indexed transcripts for
the UUID string. Direction is determined from the ordinal turn index of the UUID's first
appearance in each transcript — no transport-layer metadata required.

## Marker Format

```
<engram-src id="<uuid>"/>
```

Where `<uuid>` is a UUID v4 generated fresh for each dispatch event.

The marker is prepended to the handoff message. One UUID per dispatch — not
per session. A session that was dispatched multiple times (e.g., after a
compaction/context reset) may have multiple UUIDs, each independently linking
the session to another that shares the same UUID.

**Example handoff message:**

```
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>

Implement the scroll indicator feature. It should appear when the user
has scrolled up by more than one full viewport height, and disappear
with a fade when they scroll back to the bottom...
```

**Why XML-tag format**: AI models treat XML-style tags as passthrough metadata
rather than content to reason about, making the marker less likely to influence
model behavior.

## Direction Detection: Structural Nesting Depth

Engram determines whether a UUID was *received* or *sent* by measuring where
in the message structure it appears — not from message role, content, or any
transport-layer metadata.

**The key structural fact**: in this system's dispatch SOP, a session can only
send a UUID in one way and receive it in one way:

- **Sent**: the orchestrator embeds the UUID inside a tool call — specifically
  inside an `exec`/bash tool call whose body contains the tmux send-keys
  command. The UUID is nested inside tool_use JSON, then a shell command string.
  Nesting depth: 2 or more layers of JSON/quoting wrappers.
- **Received**: the coding agent receives the UUID as plain pasted text at the
  top level of a human-turn message. No wrappers. Nesting depth: 0.

These two cases are mutually exclusive by construction. The orchestrator
*cannot* dispatch without going through a tool call. The agent *cannot* receive
a prompt any other way than as plain text at the message surface.

At ingest, Engram measures structural nesting depth for each UUID occurrence:
count the layers of JSON object nesting, tool-call wrappers, and shell quoting
surrounding the UUID within its message object. A depth of 0 means the UUID
appears directly in the message content string. A depth ≥ 1 means it is
embedded inside tool infrastructure.

```sql
CREATE TABLE dispatch_links (
  tape_id          TEXT    NOT NULL,
  uuid             TEXT    NOT NULL,
  first_turn_index INTEGER NOT NULL,
  direction        TEXT    NOT NULL CHECK(direction IN ('received', 'sent')),
  PRIMARY KEY (tape_id, uuid)
);
CREATE INDEX dispatch_links_uuid ON dispatch_links(uuid);
CREATE INDEX dispatch_links_tape ON dispatch_links(tape_id);
CREATE INDEX dispatch_links_received ON dispatch_links(tape_id, direction, first_turn_index);
```

Ingest scans every `<engram-src>` occurrence, records the turn index, and
classifies direction from nesting depth. No message-role parsing. No content
interpretation. The direction column is a structural measurement.

**Traversal rule — causal preceding UUID**:

To traverse from a code edit at turn N in session S:

1. Find all UUIDs in S where `direction = 'received'` AND
   `first_turn_index < N`
2. Take the one with the **highest `first_turn_index`** — the most recently
   received dispatch before the edit. This is the causal upstream link.
3. Find the tape(s) where that UUID appears with `direction = 'sent'` — that
   is the parent session.
4. In the parent session, find the turn where the UUID appears (its
   `first_turn_index` in the parent, direction=sent). Call that turn M.
5. From the parent, find the most recently received UUID before turn M.
   Follow that further upstream.
6. Repeat until no further upstream sessions exist.

This handles multi-dispatch agents correctly: a coding agent may receive
many UUIDs over its lifetime (one per task), and the traversal selects
the specific dispatch that preceded the relevant code change — not just
the first dispatch ever received.

**Design principle**: UUID direction is a structural fact derived from
the dispatch mechanism itself. No inference, no heuristics, no model
cooperation required. The nesting depth of a UUID occurrence in the
transcript JSON is a deterministic reflection of whether that session
originated the dispatch or received it.

## Multi-Level Chains

The convention composes across arbitrary depth. Each handoff embeds a UUID;
each UUID independently links two adjacent transcripts.

```
Session A ──[uuid-1]──▶ Session B ──[uuid-2]──▶ Session C ──edits──▶ file.swift
              ↑                        ↑
       links A↔B                links B↔C
```

Explain resolves the full chain by iterating along `received` edges only:

1. Start with coding session C (found via file fingerprint); note the edit turn E
2. In C, find all UUIDs with `direction = 'received'` and `first_turn_index < E`;
   take the one with the highest `first_turn_index` (the causal dispatch)
3. Find the tape where that UUID appears with `direction = 'sent'` — that is
   session B (C's parent)
4. In B, note the turn M at which B sent the UUID to C; find all UUIDs in B
   with `direction = 'received'` and `first_turn_index < M`; take the highest
5. Follow that UUID upstream to session A; repeat
6. Stop when no further received UUIDs exist upstream

This ensures the traversal selects the specific causal dispatch at each hop —
critical for multi-dispatch agents that handle many tasks in the same session.
Sibling dispatches are excluded because they appear as 'sent' in the
orchestrator's transcript, never as 'received'.

Since all tapes share the same index, each UUID lookup is a flat text
search — no hierarchy traversal required.

## Query Modes

### File/span query (developer-facing)

```
engram explain <file>:<start>-<end>
```

Finds coding sessions that touched the span, then looks up dispatch markers
in those sessions to find connected sessions. Returns the full set: code
evidence plus all sessions sharing a UUID with any session in the result set.

### Dispatch query (orchestrator-facing)

```
engram explain --dispatch <uuid>
```

Entry point is the work unit, not a file location. Returns:
- All sessions containing that UUID (all connected parties in the handoff)
- All code spans touched by any session in the result set

This is the natural interface for anyone who dispatched the work and wants to
review what it produced — no file paths required.

## General Convention

The dispatch marker is not tied to any specific tool, framework, or workflow.
Any party that hands work to another party can embed `<engram-src id="<uuid>"/>`
in the handoff. Engram discovers the link at ingest time from transcript
structure alone — the position of the UUID within the message sequence
determines direction without any out-of-band metadata.

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
Human: "The barge-in behavior         <engram-src id="a3f2..."/>
  is broken — tapping during          Fix barge-in: tapping during
  speech should interrupt,            .speaking state must route to
  not cancel"                         bargeIn(), not stop()...

AI generates UUID a3f2...             Reads WatchMainView.swift
AI dispatches prompt with marker  ──▶ Edits line 229
                    UUID a3f2...      Runs build
  appears in this                     UUID a3f2... appears in
  session's tool call                 this session's first message
```

Later: `engram explain --dispatch a3f2...` returns both sessions — the full
picture from requirement to implementation. Direction was inferred from turn index at ingest time; no transport metadata
or message-role parsing was needed.

## Implementation Checklist

- [ ] Marker embedding: sending party prepends `<engram-src id="<uuid>"/>` to
      each dispatch (e.g., in orchestrator tooling)
- [ ] Ingest: extract all `<engram-src>` patterns from tapes into `dispatch_links`
      table with `first_turn_index` (ordinal index of message object where UUID first appears)
      and `direction` ('received' if UUID appears at nesting depth 0 in message content;
      'sent' if UUID appears inside tool_use/exec infrastructure at depth ≥ 1)
- [ ] Chain-explain: `engram explain` traverses upstream via causal preceding UUID:
      for each session at edit turn N, find highest-first_turn_index received UUID
      before N; follow to parent; repeat
- [ ] `explain --dispatch <uuid>` query mode
- [ ] Tests: multi-session fixture sharing UUIDs at each level
- [ ] Docs: reference in README and DESIGN.md
