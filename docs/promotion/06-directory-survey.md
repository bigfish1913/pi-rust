# Directory survey: the lists that actually take a terminal coding agent

`05-awesome-list-survey.md` checked the four lists we already knew about and found
three of the four closed by *popularity* gates. This file is the second pass: it
looks for lists that are **built for this exact category** — terminal-native
coding agents and agent harnesses — where the gate is fit rather than star count.

Two things came out of it: **the best-fit list for rpi was missing from the
earlier survey entirely**, and one more list is open but not yet reachable.

All rules below were read from the repositories themselves on **2026-09-26**
(GitHub API + raw file fetch), not from how the lists present themselves. Where a
site could not be read from this environment, that is stated rather than guessed.

## Verdict matrix

| List | Size | rpi eligible now? | The gate |
| --- | --- | --- | --- |
| [awesome-cli-coding-agents](#1-bradagiawesome-cli-coding-agents) | 1,282★ | **Yes** | fit + format only; entry goes near the bottom (sorted by stars) |
| [awesome-harness-engineering](#2-ai-boostawesome-harness-engineering) | 4,527★ | **Yes** | must answer a *harness* problem, with an opinionated 1–2 sentence note |
| [awesome-agents](#3-kyrolabsawesome-agents) | 2,841★ | **No** | open source + "demonstrated traction"; auto-closes brand-new repos |
| [AlternativeTo](#4-alternativeto--libhunt-unverified) | — | Unknown | Cloudflare-blocked from here — needs a browser |
| [LibHunt / lib.rs](#4-alternativeto--libhunt-unverified) | — | lib.rs partial | lib.rs auto-indexes crates.io; LibHunt needs a browser |

Two entries are ready to submit below. Neither list has rpi in it as of the
measurement date — re-check before opening a PR, because both accept
contributions continuously (the top list merged 25 entries from its PR queue on
2026-09-22).

---

## 1. bradAGI/awesome-cli-coding-agents

**This is the list the earlier survey should have found.** It is a curated
directory of *terminal-native* coding agents and the harnesses around them —
which is rpi's category, not a general "awesome AI" bucket.

- Repo: <https://github.com/bradAGI/awesome-cli-coding-agents>
- 1,282★, created 2026-02-07, last push 2026-09-22 (active, no licence file)
- Sections: `## Terminal-native coding agents` → `### Open Source` / `### Closed Source`,
  then `## Harnesses & orchestration` → session managers / orchestrators /
  agent infrastructure
- rpi is **not** listed.

### Rules, quoted from its `## Contributing` section

> **Inclusion requirements:**
> - Must have a **CLI or terminal interface** (IDE-only tools don't qualify)
> - Must be able to **read/write code or run commands** autonomously
> - Link must point to a **valid, active** project (no dead repos)
>
> **Entry format:**
> 1. **Name + link** (GitHub preferred)
> 2. **Star count** (for GitHub repos)
> 3. **1–2 line description** — what it does, who it's for
>
> **Optional:** provider tag `[Company]`, license, or a "why it's interesting" note
>
> Entries are sorted by GitHub stars within each section. Place your entry in the
> correct position.

rpi satisfies all three inclusion requirements. The star count must be refreshed
at submission time — the list refreshes badges on a schedule and sorts by stars,
so a stale number lands the entry in the wrong slot.

### Entry, ready for the `### Open Source` section

```markdown
- **[rpi](https://github.com/bigfish1913/pi-rust)** `⭐ <refresh at submit time>` — Library-first Rust coding agent: nine composable crates (`rpi-agent`, `rpi-tools`, `rpi-harness`, `rpi-tui`, …) so the agent loop can be embedded in your own program, plus a ready-to-run `rpi` terminal CLI. Crash-resumable JSONL sessions that self-heal a duplicated-seq log, a stable `#[repr(C)]` plugin ABI for tools and providers, and a provider-agnostic `StreamFn` boundary that makes the loop testable with no network. MIT.
```

Placement: bottom of `### Open Source` (32★ is the lowest band in that section).

**No `AGENTS.md` in this repo**, unlike `awesome-cli-apps` (see
`05-awesome-list-survey.md`), so there is no stated prohibition on agent-authored
PRs. Open it from a human account anyway: the PR body should say what rpi is in
one line and note that it is placed by star order.

---

## 2. ai-boost/awesome-harness-engineering

A curated list of resources for building reliable **agent harnesses** —
context delivery, tool design, permissions, memory and state, verification. rpi
is a harness with a written-down crash-recovery design, so it fits the
`## Reference Implementations` area rather than the general agent lists.

- Repo: <https://github.com/ai-boost/awesome-harness-engineering>
- 4,527★, active; ships `CONTRIBUTING.md`, `AGENTS.md`, `CLAUDE.md` and a
  `verify_urls.py` link checker
- Relevant sections: `## Reference Implementations` → `### Demo Harnesses`,
  and `### Memory & State` under Design Primitives
- rpi is **not** listed.

### Rules, quoted from `CONTRIBUTING.md`

> A resource belongs in this list if it:
>
> 1. **Addresses a specific harness problem** — context delivery, tool design,
>    planning artifacts, permissions, memory, verification, sandboxing, or agent
>    loop structure.
> 2. **Is worth someone's time** — not just "exists." Include a 1–2 sentence note
>    explaining why.
> 3. **Is vendor-agnostic by principle** — resources tied to a specific model or
>    platform are fine if the *pattern* generalizes.
>
> **What doesn't belong**
> - Product marketing or announcement posts without technical substance

That last line is the trap: an entry that reads like a release announcement is
excluded even though the project qualifies. The entry has to lead with the harness
problem, not the product.

`AGENTS.md` in that repo is addressed *to* AI agents contributing there, so an
agent-authored PR is anticipated. Its conventions still apply: everything in
English, one entry per line, and **no entry without a note**.

### Entry, ready for `## Reference Implementations`

```markdown
- [rpi](https://github.com/bigfish1913/pi-rust) — A Rust agent harness that treats the session log as the durable tail of an in-memory mutation state, so a crash resumes mid-turn instead of losing it. Its loader also recovers a log corrupted by two concurrent writers — duplicate sequence numbers are dropped, a divergent counter is resynced, orphaned entries are re-chained onto the lane leaf — and republishes the file atomically, which is the rare case of session integrity being documented rather than assumed.
```

---

## 3. kyrolabs/awesome-agents

- Repo: <https://github.com/kyrolabs/awesome-agents>
- 2,841★; has a `CONTRIBUTING.md`
- rpi is not listed.

### Rules, quoted from `CONTRIBUTING.md`

> The listed content should be high-quality, demonstrate traction, be maintained,
> and provide clear added value.
>
> We do not list content that is:
> - brand new repo without demonstrated traction.
> ...
>
> Given the rise of agent submissions, those criteria are non-negotiable, managed
> and applied automatically. Criteria that most often trigger closing a PR without
> merging it: brand new repo with no history, brand new user, or wrong place in
> the list.

Verdict: **not eligible yet.** rpi's first commit is 2026-08-12 and it sits at
32★; this is the same class of gate as Awesome Rust in `05-…md`, and the wording
("applied automatically") means a premature PR is auto-closed and costs standing
rather than just being ignored. Revisit when the star count is in the hundreds.

---

## 4. AlternativeTo / LibHunt (unverified)

Both were attempted and both returned HTTP 403 with a Cloudflare interstitial
("Just a moment...") from this environment, so **no rule is claimed here** — this
is exactly the case where guessing would be worse than saying nothing.

What is known without a browser:

- **lib.rs** auto-indexes crates.io, so the nine `rpi-*` crates are already
  listed there with no submission step. That is coverage we did not have to ask
  for, and it is the one directory channel currently working.
- **AlternativeTo** is the highest-value unverified channel: it is a
  *substitution* directory, so rpi's natural position is as an alternative to
  Claude Code / Codex CLI, which is the same search intent the new
  `compare.html` page targets. It needs one manual check of whether adding an
  entry requires an account, a moderation queue, or a paid tier.
- **LibHunt** indexes open-source projects by language; the Rust front
  (`rust.libhunt.com`) is the relevant one. Same manual check.

Action: open both in a browser, record the actual submission flow in this file,
and only then submit. Do not let an agent guess at a form.

---

## What this changes about the plan

1. **Submit to `awesome-cli-coding-agents` first.** It is the only list whose
   category is exactly rpi's, whose gate is fit rather than popularity, and whose
   rules are unambiguous. One PR, one line, sorted position.
2. **Then `awesome-harness-engineering`**, with the entry framed around the
   session-integrity problem — that list explicitly excludes product
   announcements, so the framing is the whole submission.
3. **Leave `awesome-agents` and Awesome Rust** until the star count moves. Both
   are popularity gates; neither is reachable by submitting harder.
4. **Check AlternativeTo by hand.** It is likely the largest untapped traffic
   source of anything in this file, and it is the one channel that pairs with the
   comparison page already published.
5. Keep measuring. Every verdict here is dated, and the two open lists merge PRs
   continuously — re-read each repo's rules before submitting, because they are
   the only source that matters.
