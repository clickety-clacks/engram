# Engram Dispatch Markers

This document describes the dispatch-marker behavior implemented by Engram.

## Marker format

Use a fresh UUID for each handoff and include it in the message sent to the
receiving agent:

```text
<engram-src id="f47ac10b-58cc-4372-a567-0e02b2c3d479"/>
```

The sender's transcript records the marker in the tool call or dispatch
arguments. The receiving transcript records the same marker in the message it
receives. Engram extracts marker-shaped UUIDs from supported transcript rows
when it ingests them.

## Direction and first occurrence

Engram classifies a marker on the message surface as `received`. A marker found
only inside a tool call's input or arguments is `sent`. The extractor
recognizes the supported native and normalized tool-call rows; quoted tool
result output in a native Claude user envelope is not treated as a received
marker. This classification follows transcript structure, not timestamps or
similarity between messages.

Engram stores the first occurrence of each UUID in each tape. A later repeat
does not renew the marker or change its direction. If the same UUID appears
both as a sent and received marker at the same turn, the surface occurrence is
recorded as received.

For an appended native session, Engram follows the immutable predecessor
segments and folds their first occurrences before selecting a handoff. A later
segment cannot make an older marker look new. Recovery keeps the context tape,
marker segment, and original edit tape bound together; output attributes the
edit to its original evidence tape.

## When explain reports a handoff

`explain` follows dispatch markers as part of its normal lineage traversal; it
has no separate `--dispatch` mode. For an edit, Engram selects the latest
received first occurrence before that edit in the reconstructed history. It
then searches the selected stores for independent histories whose first
occurrence of the same UUID is sent.

- One matching independent sender can produce a handoff, with both endpoint
  locations in the result.
- No sender, a missing segment or tape, or incomplete owner evidence leaves the
  handoff unresolved. Engram does not invent an endpoint.
- More than one independent sender is ambiguous; Engram reports the ambiguity
  and does not choose by comparing turn numbers from separate conversations.
- Cycles are bounded and reported; traversal does not infer a parent from
  timestamps, text similarity, or marker counts alone.

Queries are local unless the caller explicitly selects peers with `--peers`.
The selected stores define the evidence scope. If a selected peer fails,
completed evidence remains in the result and the source reports partial
coverage with its observed phase and reason.

## Example

An agent on `laptop-a` sends a task to an agent on `build-host` and includes a
fresh marker in the dispatch arguments:

```text
<engram-src id="8dfcc36a-75a0-4d18-9e50-8da567e0a44d"/>
```

The receiving transcript on `build-host` contains the same marker. After the
agent edits `src/auth.rs`, both machines ingest their own transcripts. If the
querying checkout contains that source span, the caller can ask:

```bash
engram explain src/auth.rs:40-55 --peers build-host
```

The result can locate the edit in `build-host/default` and link it to the
sender in the caller's selected local stores. The link appears only when the
receiver's earlier `received` marker and one independent sender's `sent`
marker are both present in the selected evidence.
