+++
id = "01M32EH22Q120T4N8DKB1XWFNE"
layer = "reference"
title = "future ideas (not core, not scheduled)"
status = "draft"
agent_policy = "editable"
owner = "caleb"
tags = ["journal", "future", "not-core"]
parent = "kb:01M32EH22Q4MFV9P72M6Y7CRKY"

[access]
read = ["*"]
write = ["owner"]
+++

# future ideas (not core, not scheduled)

this file is a dump of thoughts that are out of scope for the current build. nothing here is a commitment, a requirement, or part of the design in [design.md](design.md). ideas move out of this file only by being written into the design with a build step.

## coordination

- **claims and a coordinator**: a lease-based claim on document ids so two agents never open competing edits. cut because git worktrees plus merge-time review cover the same failures at lower cost. revisit if semantic duplication (two agents appending the same fact) shows up in nightly curation more than a few times a month.
- **live "who is editing what"**: an agent discovery and message-passing layer where each document has one editor at a time, similar to how multi-agent harnesses hand off work. the derived list of open worktrees is the cheap version; a real protocol is future.
- **postgres for shared state**: only if a single-writer sqlite file per deployment stops being enough.

## access and identity

- **per-group read filtering in retrieval**: `access.read` is stored but not enforced in search. needed for a multi-tenant server, not for a local or single-team repository.
- **ownership transfer**: an operation for moving `owner` when a person leaves. today it is a human edit.
- **token issuance and rotation**: service principals with bearer tokens, per-token scopes, revocation.
- **restricted-content repositories**: if any content must be hidden from people with clone access, it goes in a separate repository. the design assumes one repository with no restricted content.

## scale

- **captures as their own repository or store shard**: the content store shards by hash prefix already; a second store or repository is a later split.
- **crdt or live collaborative editing**: character-level concurrent editing would be an editor writing files into this repository. nothing in the design changes.
- **kubernetes and horizontal workers**: the server mode is stateless workers plus one git remote and one content store. an orchestration layer is deployment detail, not design.
- **cross-region blob replication and restore drills**: worth doing once the store holds anything that cannot be re-captured.

## retrieval and ranking

- **usage signals in ranking**: reads and cites as rerank features. decide by the eval set after 90 days of usage data.
- **hosted vs local rerank model**: decide after hybrid retrieval is measured.
- **tuning criticality weights**: defaults in the design; tune against labeled importance after usage data exists.
- **learned near-duplicate threshold**: the 0.92 cosine default is a guess; label a pair set.

## editing surfaces

- **editor plugin** showing the derived block and section blame inline.
- **web review ui** for review-class items when there is no git host.
- **compaction ux**: a view that shows the checkpoint lineage so a reader can see what was squashed.

## agents

- **promotion between layers as an automated proposal** (procedure to decision to design to principle after quiet periods). the mechanism exists as `escalate` plus a human; a scheduled proposer is future.
- **multi-provider routing per hook role** with cost and quality tracking.
- **opentelemetry export** of run logs once the genai semantic conventions are stable.
- **replay and fork of runs** from the logged request deltas as a debugging tool.

## observations to revisit

- percentile-based criticality always produces a bottom band; the corpus floor of 50 is a stopgap. an absolute importance floor may be better.
- the review agent's approval rate is the thing to watch once auto-merge is on; a rubber-stamping reviewer is invisible without a sampled human audit.
