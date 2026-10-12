# 011: Agent panel

Status: not started · 2026-10-11

## Problem

An agent that can read the graph and propose changes, with every change reviewable before it
lands.

## Principles

III, IV (agent writes go to a branch, never straight to a protected one), VII.

## Notes for when this starts

- Host agents over the Agent Client Protocol (`agent-client-protocol`, Apache-2.0), as Zed's
  agent panel does; Zed's `acp_thread` and `agent_ui` are candidates to depend on or fork.
- Tool calls render as cards with a status (pending, waiting for approval, running, done,
  failed); writes wait on an approval with Allow once, Always for this branch, Reject.
- Agent writes go to a branch the agent owns and arrive in review (spec 008) like any other.
- The palette's "Ask" (spec 002) hands text to the agent.
