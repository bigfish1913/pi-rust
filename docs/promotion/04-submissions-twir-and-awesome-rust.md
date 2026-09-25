# Third-party submissions: This Week in Rust, Awesome Rust

> Both entries below were checked against each project's own current rules on
> 2026-09-25, because the obvious route for each one **no longer works**.
> Re-read their docs before submitting — these gate on participation and on
> popularity thresholds that rpi does not currently meet.

---

## 1. This Week in Rust

### The route I originally assumed does not exist

TWiR **no longer accepts pull requests** for its Project/Tooling Updates
section (rust-lang/this-week-in-rust#8575). From their README:

> We are no longer accepting pull request submissions for the Project/Tooling
> Updates section. Our editors monitor r/rust and will consider links that are
> posted there.

So there is no submission to make. **The actionable step is posting on r/rust**
— see [`03-english-showhn-devto-reddit.md`](03-english-showhn-devto-reddit.md),
r/rust section. Getting picked up is the editor's call, not something to request.

Do not open an issue or PR asking to be featured. It will be closed.

### What TWiR looks for

From their inclusion guidelines, the framing that counts is long-form and
explanatory:

- how-to intros and advanced deep dives into Rust concepts
- walkthroughs that explain things differently from the Rust book / Rustlings /
  Rust by Example
- **tooling updates framed as a tutorial** (a release announcement alone is not
  it)
- observations and thoughts on Rust and the community
- calls for participation in Rust open source projects

The article that fits is
[`02-rust-agent-runtime-architecture.md`](02-rust-agent-runtime-architecture.md)
— specifically the sections on why the plugin ABI is a hand-written `#[repr(C)]`
contract and why crash recovery records frames rather than turns. Those are
Rust design explanations, not product announcements.

### Crate of the Week

Crates are nominated and voted on in a forum thread, not in the issue tracker:

<https://users.rust-lang.org/t/crate-of-the-week/2704>

A nomination is a short forum post, not an article. Draft:

> I'd like to nominate **rpi-agent** for Crate of the Week.
>
> It is the agent-loop layer of a coding-agent runtime, provider-agnostic by
> construction: the only model boundary is a `StreamFn` that returns a streaming
> event handle synchronously, so a real provider, a deterministic in-process test
> double and a recorded transcript are interchangeable. Provider failures are
> reported as `Error` events in the stream rather than `Err`, which keeps the
> loop's failure handling to a single path.
>
> The loop's documented invariants are the interesting part: tools may finish in
> any order but results are emitted in call order; a `MessageEnd` is emitted
> exactly once per assistant message even when a late streaming update arrives
> after the run settled; and a truncated tool call never leaves a call without a
> result — because no provider will accept that conversation.
>
> It is MIT licensed and part of the `rpi` family
> (<https://github.com/bigfish1913/pi-rust>), but the crate stands on its own as
> an embeddable loop.

Nominate it once. Repeatedly nominating the same crate is the fastest way to get
ignored.

### ⚠️ LLM authorship disclosure

TWiR's README has an explicit policy:

> We don't take a position on whether or not you use LLMs. We do care whether
> articles submitted to TWiR were written by people. […] If you submit an
> LLM-written article to TWiR, we request that the LLM authorship be disclosed in
> the article.

The drafts in this directory were produced with AI assistance. Before anything
from this directory goes to TWiR — or anywhere that reads like a personal essay —
either:

1. **rewrite it in your own voice and own it as your writing**, keeping the
   facts (all the numbers and code in these drafts are verifiable against the
   repository), or
2. **disclose the AI assistance** in the article.

Option 1 is what TWiR is actually asking for: they want an author who can engage
with readers and grow from the feedback. Publishing AI-assisted text as personal
writing without disclosure risks a bad interaction with a community that checks.

---

## 2. Awesome Rust

### Not eligible yet — this is a threshold, not a judgment call

`rust-unofficial/awesome-rust` accepts entries that meet **either**:

```
stars > 50   OR   downloads > 2000
```

Measured on 2026-09-25:

| Metric | Current | Threshold | Eligible? |
| --- | --: | --: | --- |
| GitHub stars | 32 | > 50 | ❌ |
| Best crate downloads (`rpi-telemetry`) | 1,112 | > 2,000 | ❌ |
| `rpi-cli` downloads | 469 | > 2,000 | ❌ |

**Do not submit yet.** A PR that ignores a stated numeric gate is a bad first
impression, and the maintainers explicitly say they will not make exceptions.
Revisit when `rpi-cli` (the crate a user would actually install) passes 2,000
downloads, or when the repository passes 50 stars — whichever comes first.

Their TL;DR also asks for "mostly projects that are stable and useful to many
users", which argues for the repo settling for a while before the submission.

### The entry, ready for when it qualifies

Exact template, from their CONTRIBUTING.md:

```
[ACCOUNT/REPO](https://github.com/ACCOUNT/REPO) [[CRATE](https://crates.io/crates/CRATE)] - DESCRIPTION
```

Entry to paste (with the CI badge, which they also ask for):

```markdown
* [bigfish1913/pi-rust](https://github.com/bigfish1913/pi-rust) [[rpi-agent](https://crates.io/crates/rpi-agent)] - Library-first coding-agent runtime: composable provider, agent-loop, tool and session crates, plus a terminal agent and a stable `#[repr(C)]` plugin ABI. [![CI](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml)
```

**Where it goes:** `README.md` → `### Artificial Intelligence`. That section has
no generic agents subsection; the agent runtimes currently live under
`#### OpenAI` (e.g. `openai/codex`, `0xplaygrounds/rig`, `awakenworks/awaken`,
`bigduu/Bamboo-agent`). That is the least-wrong fit for a provider-agnostic
runtime — but if a maintainer moves it to `#### Tooling`, that is a better
placement and not worth arguing.

**Sort order:** entries are alphabetical by the `ACCOUNT/REPO` text, so it lands
between `bigduu/Bamboo-agent` and `liquidos-ai/AutoAgents`. Check the neighbours
in the file at submission time rather than trusting this line.

### Process

Their preferred flow is the web UI, not a script:

1. Open <https://github.com/rust-unofficial/awesome-rust/blob/main/README.md>
2. Click the pencil ("edit") icon
3. Insert the entry in alphabetical position
4. Open the PR, and state the qualifying metric (stars or downloads) in the PR
   description — they ask for it, and it is the thing being checked

---

## Summary of what is actually actionable now

| Channel | Status | Action |
| --- | --- | --- |
| r/rust | ready | Post from `03-...md`. This is also how TWiR finds you. |
| r/LocalLLaMA | ready | Post from `03-...md`. |
| DEV.to / Hashnode | ready | Post from `03-...md`, after HN. |
| Lobsters | ready, gated on account standing | Post from `03-...md`, with the disclosure line. |
| Hacker News | ready | Post from `03-...md`. |
| This Week in Rust | no submission route | Post on r/rust; optionally nominate the crate in the forum thread. |
| Awesome Rust | **blocked by threshold** | Revisit at 2,000 downloads or 50 stars. |
