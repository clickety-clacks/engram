# Engram Dispatch Marker

Status: Draft

## Problem

Engram can explain which agent sessions touched a span of code. It cannot explain
*why* those sessions existed — what orchestrator conversation originated the work.

In vibe-coding workflows, the reasoning behind a code change lives in the
orchestrator conversation (e.g., a CLU stream discussing a bug or feature), not
in the coding agent session that made the edit. These are separate transcripts
with no structural link between them.

## Solution

A dispatch marker is a UUID embedded by the orchestrator in the prompt it sends to
a coding agent. Because the prompt becomes the first message in the agent's
transcript, the UUID appears verbatim in both:

1. The orchestrator's tape (as part of the tool call / message that sent the prompt)
2. The agent's tape (as the opening text of the session)

No coordination between orchestrator and Engram is required at dispatch time.
The link is discovered at query time by pattern-matching the UUID across indexed
tapes.

## Marker Format

```
[engram:src=<uuid>]
```

- `<uuid>` is a freshly generated UUID v4, unique per dispatch
- The marker is prepended to the agent prompt text
- The UUID identifies the specific dispatch event, not the orchestrator session

**Example prompt:**

```
[engram:src=f47ac10b-58cc-4372-a567-0e02b2c3d479]

Fix the scroll-to-bottom indicator in ChatView.swift. The indicator
should appear when the user is scrolled up by more than one screen height...
```

## Why UUID, Not Session Key

A session key identifies the orchestrator session as a whole, which may contain
discussions about many unrelated topics. The dispatch UUID identifies the specific
work unit — the moment the orchestrator decided to dispatch this particular task.
This gives `explain` the right granularity: the CLU context around that specific
dispatch, not the entire conversation.

## Ingest Behavior

No changes required. The marker is plain text and is indexed by existing adapters
as part of `msg.in` / `msg.out` events. Both the orchestrator tape and the agent
tape naturally contain the UUID string.

## Explain Behavior

When `engram explain <target>` returns a set of sessions, the explain query
additionally:

1. Scans each result session's tape events for the pattern `[engram:src=<uuid>]`
2. For each UUID found, searches all indexed tapes for the same UUID string
3. Runs explain on any tapes containing the UUID (the orchestrator tapes)
4. Merges the orchestrator sessions into the explain result, annotated as
   `tier: orchestrator`

This chain is one level deep by default. Recursive chaining (orchestrator →
meta-orchestrator) is not required initially but is not precluded by this design.

## General Convention

The dispatch marker is not specific to CLU or any particular orchestrator. Any
agent that dispatches work to another agent can embed `[engram:src=<uuid>]` in
the prompt. Engram will discover and surface the link without needing to know
anything about the orchestrator's identity or session structure.

## Open Questions

- Should the explain result distinguish between "agent touched this code" and
  "orchestrator originated this work"? (Likely yes — `tier` field in result.)
- Should a single agent session be allowed to have multiple dispatch UUIDs (e.g.,
  one per compaction boundary)? (Yes — the marker appears in each resumed
  session's opening context.)
- Recursion depth: should explain follow chains deeper than one tier if an
  orchestrator was itself dispatched by a meta-orchestrator? (Out of scope for
  initial impl.)

## Implementation Checklist

- [ ] Marker embedding: orchestrator prepends `[engram:src=<uuid>]` to every
      dispatch prompt (e.g., in `submitter-eezo`)
- [ ] Chain-explain: `engram explain` scans result tapes for marker, pattern-
      matches UUID across all indexed tapes, merges orchestrator results
- [ ] Result schema: add `tier` field to session results (`agent` | `orchestrator`)
- [ ] Tests: end-to-end fixture with orchestrator tape + agent tape sharing a UUID,
      verify explain surfaces both
- [ ] Docs: document the convention in README / DESIGN.md
