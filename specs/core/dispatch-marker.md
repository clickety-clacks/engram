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

## Direction Detection: First-Turn-Index

Engram determines direction from a structural fact about when the UUID first
appears in each session's transcript — not from message role or content.

**The invariant**: the sender generates the UUID mid-conversation (at a high
turn index) and the receiver gets it as its task prompt (at a low turn index).
This holds by construction: you cannot receive a UUID before it is generated.

At ingest, Engram records the turn index of the first occurrence of each UUID
in each tape — where "turn index" is the ordinal position of the message object
in the transcript array (0-based). No content interpretation is required; this
is a structural measurement of the conversation array.

```sql
CREATE TABLE dispatch_links (
  tape_id          TEXT    NOT NULL,
  uuid             TEXT    NOT NULL,
  first_turn_index INTEGER NOT NULL,
  PRIMARY KEY (tape_id, uuid)
);
CREATE INDEX dispatch_links_uuid ON dispatch_links(uuid);
CREATE INDEX dispatch_links_tape ON dispatch_links(tape_id);
```

Ingest scans every `<engram-src>` occurrence in every tape and records the
index of the message object containing the first occurrence. The message role
(human vs assistant) is irrelevant — only the ordinal position matters.

**Traversal rule (query-rooted BFS + first-UUID pruning)**:

From any session S in the result set, follow only S's UUID with the
**lowest `first_turn_index`**. This is S's received UUID — the one it was
given at the start of the task. UUIDs at higher turn indices are UUIDs S
dispatched outward; they are not followed.

This ensures the traversal walks strictly upstream. When the algorithm reaches
an orchestrator session, it follows the UUID the orchestrator *received*
(further upstream) and ignores all UUIDs the orchestrator dispatched to other
sessions (siblings of the starting session, unrelated to the current query).

**Design principle**: UUID presence and `first_turn_index` in a tape are facts.
Direction is recovered by comparing turn indices across tapes sharing a UUID —
the structurally earlier occurrence is the receiver. No message-role parsing,
no content interpretation, no transport metadata required.

## Multi-Level Chains

The convention composes across arbitrary depth. Each handoff embeds a UUID;
each UUID independently links two adjacent transcripts.

```
Session A ──[uuid-1]──▶ Session B ──[uuid-2]──▶ Session C ──edits──▶ file.swift
              ↑                        ↑
       links A↔B                links B↔C
```

Explain resolves the full chain by iterating along `received` edges only:

1. Start with coding session C (found via file fingerprint)
2. Find the UUID in C with the lowest `first_turn_index` (C's received UUID)
3. Find all tapes where that UUID also appears
4. Add new sessions to result set; for each newly added session, go to step 2
5. Repeat until no new sessions are added

Pruning to the lowest-index UUID per session ensures the traversal only
walks upstream: each session contributes only the UUID it was *given*, not
the UUIDs it dispatched to other sessions. This prevents sibling dispatches
from polluting the result set.

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
- [ ] Chain-explain: `engram explain` expands along `received` edges only,
      iterating until result set is stable
- [ ] `explain --dispatch <uuid>` query mode
- [ ] Tests: multi-session fixture sharing UUIDs at each level
- [ ] Docs: reference in README and DESIGN.md
