# Promotion content

Ready-to-publish material for `rpi`. Every number and code sample here is
verifiable against this repository.

| File | What it is | Where it goes |
| --- | --- | --- |
| [`01-benchmark-vs-native-pi.md`](01-benchmark-vs-native-pi.md) | The measured comparison against native TypeScript `pi`, in Chinese and English | 掘金 / OSCHINA / RustCC / HN |
| [`02-rust-agent-runtime-architecture.md`](02-rust-agent-runtime-architecture.md) | Long-form Chinese article on the layering, the provider seam, the plugin ABI and crash recovery | 掘金 / OSCHINA / 知乎 |
| [`03-english-showhn-devto-reddit.md`](03-english-showhn-devto-reddit.md) | English posts | Hacker News, DEV.to / Hashnode, r/rust, r/LocalLLaMA, Lobsters |
| [`04-submissions-twir-and-awesome-rust.md`](04-submissions-twir-and-awesome-rust.md) | Verified submission rules for two third-party lists | This Week in Rust, Awesome Rust |
| [`05-awesome-list-survey.md`](05-awesome-list-survey.md) | Five curated lists checked against their written rules, with the exact gate on each | Reference: which lists are open now, when the rest open |

The short posts, per-platform framing notes and the pre-post checklist live in
[`../promotion-kit.md`](../promotion-kit.md). That file is the index; these are the
long pieces.

## Two things to read before publishing anything

**1. TWiR requires LLM-authorship disclosure.** These drafts were written with AI
assistance. This Week in Rust's README asks that LLM authorship be disclosed in
any article submitted to it, because "TWiR exists as part of a broader Rust
community" and an LLM author cannot engage with or learn from that community. So
either rewrite a piece in your own voice and own it, or disclose. Details in
`04-...md`.

**2. Awesome Rust is not yet eligible.** It gates on `stars > 50` or
`downloads > 2000`; rpi is at 32 stars and 1,112 downloads on its best crate
(`rpi-telemetry`). Submitting before then ignores a stated rule. Details in
`04-...md`.

**3. Three of the four awesome-lists are closed for reasons unrelated to our
effort, and one has a policy aimed at coding agents.** `awesome-cli-apps` is the
only one a human can submit to today; `awesome-tuis` has a mechanically-enforced
6-month repo-age rule that opens around 2027-02-12. Read
`05-awesome-list-survey.md` before preparing any entry — every gate lives in a
contributing guide, a PR template or an `AGENTS.md`, and none is visible from the
rendered README.

## Publishing order

Post the same text everywhere and it reads as spam. Space them out:

1. **Hacker News** first — it is the highest-variance channel, and a good thread
   gives you answers to reuse everywhere else.
2. **DEV.to / Hashnode** a day or two later, expanded as a tutorial.
3. **r/rust**, then **r/LocalLLaMA** — different angles, not the same text.
4. **中文社区** (掘金 / OSCHINA / 知乎) on separate days, 3–5 apart, each with a
   different lead section.
5. **Lobsters** last, and only with account standing; it punishes self-promotion
   harder than any other channel here.

Leave at least 3–5 days between posts to the same community, and never post the
same article to two communities on the same day.
