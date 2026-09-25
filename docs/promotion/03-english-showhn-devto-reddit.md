# English launch posts — Hacker News / DEV.to / Reddit / Lobsters

> Each section is copy-pasteable as-is. Post them **days apart**, not the same day:
> HN first, then DEV.to, then Reddit and Lobsters. Adjust the tone to the venue
> rather than cross-posting one text everywhere.

---

## 1. Hacker News (Show HN)

**Title**

```
Show HN: rpi – A Rust-native coding-agent runtime that is usable as a library
```

**Body**

```
rpi is a coding-agent runtime in Rust. The part I care about is that the agent
loop is a library, not a CLI you shell out to and scrape: rpi-agent + rpi-ai is
enough to run an agent inside your own process with your own event handling.

Highlights:

- Streaming agent loop with tool calls, hooks, queues and cancellation.
- Anthropic Messages, OpenAI Chat Completions/Responses, plus OpenRouter,
  DeepSeek and llama.cpp gateways. The default build pulls no HTTP stack —
  providers are feature-gated — so the test suite cannot silently depend on the
  network. There is a deterministic faux provider for offline tests.
- Sessions are a tree with JSONL persistence and crash recovery driven by
  frame-level progress records. A process that dies mid-tool-call resumes instead
  of losing the turn or re-running the tool.
- Plugins are Rust cdylibs behind a hand-written #[repr(C)] ABI with version
  negotiation and panic containment, because host and plugin are compiled
  separately and may use different compiler versions.
- Remote mode: a headless server plus a connect client that holds no local agent
  state.

I also measured it against the TypeScript original it ports, same machine, same
RPC endpoint, isolated config, both offline:

  --version        17.9 ms vs 172.9 ms   (9.7x)
  RPC ready       103.6 ms vs 176.4 ms   (1.7x)
  RSS at ready     18.6 MiB vs  91.6 MiB (4.9x)
  install        23.9 MiB vs  ~513 MiB   (~21x)

The honest caveat: most of rpi's remaining 103 ms is its *own* initialisation,
not process startup — about 86 ms of it. Pi's cost is almost entirely Node boot,
so it has little left to win there. So the next work is lazy initialisation,
not a smaller binary. I have not measured tool-loop throughput or long-session
memory for either tool.

Known gaps, stated up front: the Node/TypeScript extension bridge is opt-in and
incomplete, Intel macOS prebuilt binaries are not built yet, and
docs/native-pi-missing-features.md tracks the compatibility gaps rather than
hiding them.

  curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
  # or: cargo install rpi-cli

https://github.com/bigfish1913/pi-rust

MIT licensed. Happy to answer questions about the crate split, the plugin ABI, or
the crash-recovery model.
```

**Notes**

- Post on a weekday morning US Eastern. Stay in the thread for the first hours.
- Expected pushback: "why another agent" and "why does the benchmark not measure
  LLM work". Answer both plainly — the post already says what is not measured,
  and repeating that is stronger than defending.
- Do not ask anyone to upvote.

---

## 2. DEV.to / Hashnode

**Title**

```
Building a testable coding agent in Rust
```

**Tags**: `rust`, `ai`, `opensource`, `testing`

**Body**

```
Most coding agents are CLIs. If you want one inside your own process — an editor
plugin, a CI job, an internal tool — your only option is to shell out and parse
stdout. That works until it doesn't: the output format is meant for humans, error
handling is string matching, and you cannot subscribe to intermediate events.

This is a walkthrough of the other approach: an agent loop that is a library, with
the tests to prove it. Everything below runs offline, with no API key.

## Start with a deterministic provider

The single most useful decision was making the model boundary one small trait, and
shipping a fake implementation of it. Every example and every test starts like
this:

```rust
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;

let provider = FauxProvider::new(
    FauxScript::new()
        .with_tool_call("write", serde_json::json!({
            "path": "out.txt",
            "content": "hello"
        }))
        .with_text("Done."),
);
```

A tool call followed by a text turn exercises the entire loop: the call is
validated, executed, its result is fed back, and a second provider turn runs. No
network, no key, no flakiness, and the test pins exact model behaviour instead of
mocking it.

## Put the filesystem behind a trait

The tools do not touch the real filesystem. They go through `ExecutionEnv`:

```rust
use rpi_tools::{create_read_tool, ExecutionToolContext, InMemoryExecutionEnv};

let env = Arc::new(InMemoryExecutionEnv::new());
let ctx = ExecutionToolContext::new(
    env.clone() as Arc<dyn rpi_tools::ExecutionEnv>,
    Some(env as Arc<dyn rpi_tools::MutatingEnv>),
);
let read = create_read_tool(&ctx, None);
```

The same `read` tool runs against `OsExecutionEnv` in production. The benefit is
not only "no network" — it removes a whole class of flaky failure, where parallel
tests fight over the same relative path on disk.

## Derive tool schemas, do not hand-write them

Hand-written JSON Schema drifts from the implementation. Deriving it from the Rust
type with `schemars` means it cannot. The other half is that models emit
almost-valid JSON — trailing commas, single quotes, booleans as strings — so there
is a coercion pass before validation rather than a hard rejection.

## Persist progress, not just turns

This is the part I would carry into any agent implementation. A turn can take
minutes. If you persist only at turn boundaries, a crash at second 55 loses
everything, including the user's prompt.

But persisting each turn as history is also wrong: the assistant message may be
half-streamed and a tool may be mid-execution. Treating that as "already happened"
means re-running side-effecting tools on restart.

The model that works:

- record **streamed frames as progress, never as history** — they never enter the
  branch the model is built from;
- commit an assistant message with tool calls **the moment it settles**, recording
  a step attempt, so the tool-start frame can be written before the tool runs;
- on restart, either replay an unresolved tool call (only when both the recorded
  policy and the current tool declaration mark it safe) or record it as
  "outcome unknown" — because no provider accepts a tool call without a result;
- record retry intent separately, so crashing during backoff retries instead of
  salvaging the failed attempt as history;
- bound every resume path with a budget, so a run that keeps dying cannot restart
  the same work forever.

And the recovery should not silently re-run. Repair the session to a valid state,
then let the caller decide when to continue — driving a run has to happen wherever
the output is rendered.

## What this costs

More upfront design than a "just call the API in a loop" script. The payoff is
that the loop is testable, the failure modes are explicit, and the same code can
back a CLI, a server and an embedded agent.

If you want to try the finished version:

```bash
curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
# or: cargo install rpi-cli
```

Runnable offline example: `cargo run -p minimal`.

Repo: https://github.com/bigfish1913/pi-rust (MIT)
```

---

## 3. Reddit

### r/rust

**Title**

```
[Showcase] rpi: a Rust coding-agent runtime split into nine composable crates
```

**Body**

```
rpi is a coding-agent runtime in Rust, published as nine crates with one-way
dependencies:

  rpi-telemetry -> rpi-ai -> rpi-agent -> rpi-tools -> rpi-harness -> rpi-cli
                    ^                                        ^
              rpi-tui                          rpi-plugin-sdk -> rpi-extensions

Design decisions that might be interesting here:

- The LLM boundary is one trait (`StreamFn` -> `Provider::stream_simple`), so
  providers, a test double and a recorded transcript are interchangeable. Provider
  failures are reported as `Error` events in the stream rather than `Err`, so the
  loop has a single failure path.
- `rpi-ai`'s HTTP providers are behind a feature flag, so the default build
  contains no network stack and the test suite cannot accidentally depend on one.
- The plugin SDK is a **zero-dependency leaf**. Plugins are cdylibs the host loads
  with libloading, so if the SDK depended on the runtime the plugin would link the
  host — which breaks. The ABI is a hand-written `#[repr(C)]` contract: no `Drop`
  types cross, strings cross as ptr+len with an explicit `free_string` (each
  allocation freed exactly once, by the receiver), structured data crosses as JSON,
  and both sides contain panics with `catch_unwind` because unwinding out of an
  `extern "C"` boundary aborts the process.
- Tool parameters are derived from Rust types with `schemars` rather than
  hand-written JSON Schema, plus a coercion pass for the almost-valid JSON models
  actually emit.

I also measured startup against the TypeScript original it ports, same machine,
same RPC endpoint, isolated config, both offline: 9.7x faster to start, 1.7x
faster to a usable agent, 4.9x smaller RSS. The interesting part is that most of
rpi's remaining 103 ms is its own initialisation (86 ms), not process start — so
the optimisation target is lazy init, not a smaller binary.

Honest gaps: the JS/TS extension bridge is opt-in and incomplete, and
`cargo fmt --all` does not currently pass across the workspace (tracked in #15).

Repo: https://github.com/bigfish1913/pi-rust (MIT). Feedback on the crate split
especially welcome.
```

### r/LocalLLaMA

**Title**

```
rpi: a Rust coding-agent runtime with pluggable providers and fully offline tests
```

**Body**

```
Sharing a coding-agent runtime I have been building in Rust. Relevant if you run
models behind an OpenAI-compatible endpoint.

- Providers: Anthropic Messages, OpenAI Chat Completions/Responses, and
  OpenAI-compatible gateways including OpenRouter, DeepSeek and llama.cpp. Custom
  endpoints go in a small `models.json` (`baseUrl`, `apiKey`, headers), so a local
  server works without patching anything.
- A deterministic faux provider means the whole test suite and the examples run
  with no key and no network. `cargo run -p minimal` gets you a working agent in
  one command.
- Durable sessions: JSONL, branching, compaction, and crash recovery — a process
  that dies mid-tool-call resumes rather than losing the turn.
- Local tooling is behind an `ExecutionEnv` trait, so tools run against the real
  filesystem or an in-memory one; the in-memory backend is what the tests use.

Not measured: real token throughput or latency, since that is a property of the
provider and your hardware, not the agent.

https://github.com/bigfish1913/pi-rust (MIT)

  curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
```

**Notes**

- r/LocalLLaMA dislikes unlabelled self-promotion: if it asks for an "AI" flair or
  a specific tag, use it.
- Expect "does it work with <my server>" questions. The honest answer is: any
  OpenAI-compatible endpoint via `models.json`, and llama.cpp/DeepSeek are
  first-class presets.

---

## 4. Lobsters

**Title**

```
rpi: a Rust-native, library-first coding-agent runtime
```

**Body** (keep it short; Lobsters rewards substance over marketing)

```
A coding-agent runtime written in Rust, split into nine crates with one-way
dependencies, plus a terminal agent on top.

The design choice worth discussing: rather than a CLI with an API bolted on, the
agent loop is a library. The model boundary is a single trait, so providers, a
deterministic test double and recorded transcripts are interchangeable, and the
default build contains no HTTP stack at all — providers are feature-gated so the
test suite cannot depend on the network by accident.

Plugins are cdylibs behind a hand-written `#[repr(C)]` ABI. Host and plugin are
compiled separately with potentially different compiler versions, so no `Drop`
type may cross the boundary: strings cross as ptr+len with an explicit free
function, structured data crosses as JSON, and both sides contain panics with
`catch_unwind` because unwinding across `extern "C"` aborts the process.

https://github.com/bigfish1913/pi-rust

(Disclosure: this is my project.)
```

**Notes**

- Lobsters is strict about self-promotion: disclose it in the post, and only submit
  if you have an account with enough standing. If the submission is declined,
  do not resubmit.
