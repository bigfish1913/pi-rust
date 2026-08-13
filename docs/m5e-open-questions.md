# M5e (pi-harness skills / prompt-templates / system-prompt / events) open questions & divergences

> Written per the user's instruction "中间有问题，写文档，我睡醒统一处理"
> (if problems arise mid-way, write them to a doc; I'll review them on waking).
> These are the design divergences from the TS reference that the M5e ports
> introduced. None are blockers — all are recorded for review.

## 1. YAML-subset frontmatter parser instead of `serde_yaml`

**Where:** `crates/pi-harness/src/frontmatter.rs` (used by `skills.rs` and
`prompt_templates.rs`).

**What.** The TS loaders parse the fenced frontmatter block with the `yaml` npm
package. This port uses a hand-rolled minimal YAML-subset parser
(`parse_frontmatter`/`parse_simple_yaml`/`parse_yaml_value`) to avoid a
`serde_yaml` dependency, consistent with the workspace's minimal-dep posture
(already established in M4/M5c: no `serde_yaml`, no `regex`, no `ignore`).

**Coverage.** The subset handles exactly the frontmatter shapes that occur in
practice:
- `key: value` lines (top-level mapping only — no nested block mappings);
- scalar values: string, `true|True|TRUE`/`false|…` bools, `null|Null|NULL|~`,
  integers, floats-with-`.`;
- flow collections `[a, b, c]` and `{k: v, k2: v2}`;
- double-quoted strings (with `\"`/`\\`/`\n`/`\t` unescaping) and single-quoted
  strings (`''` escaped quote); plain scalars otherwise.

**Divergences / limitations (NOT handled):**
- **Block sequences** (`- item` indented lines) — not parsed; would need a
  recursion level the subset doesn't model. No skill/template frontmatter in the
  reference uses them.
- **Block mappings** (nested indented `key: value`) — flat only. Again unused by
  the reference frontmatter.
- **Anchors / aliases / multi-document** — out of scope.
- **Inline trailing comments are NOT stripped.** A line `description: See http://x/#frag`
  keeps the full URL. The TS `yaml` parser strips `# …` inline comments; our
  parser intentionally does not, because naive stripping breaks URL/hash values.
  If a frontmatter value ever needs an inline comment, this will diverge. Full-line
  `#` comments ARE skipped.
- **Quoted-key flow mappings** (`{"a": 1}`) — keys are taken as bare scalars;
  quotes around flow-mapping keys are not stripped. Unused in practice.

**Error parity.** Unterminated flow sequence/mapping and unterminated quoted
strings yield `Err(message)`, which the loaders surface as a `parse_failed`
diagnostic — matching the TS behavior for the common malformed-frontmatter case
(the reference test `description: [unterminated → parse_failed`). Other YAML
errors the full `yaml` parser would catch (e.g. a tab-indentation error) are not
flagged here; they'd parse as a flat scalar or a missing-colon error.

**Resolution for review:** acceptable for v1. If real-world skill/template
frontmatter ever needs block sequences or nested mappings, either (a) pull
`serde_yaml` (one-line dep, well-maintained) or (b) extend the subset. The
hand-rolled parser was chosen to keep the dep surface minimal and because the
reference frontmatter is uniformly flat.

## 2. `load_sourced_skills` / `load_sourced_prompt_templates` lose the `mapSkill` hook

**Where:** `crates/pi-harness/src/skills.rs::load_sourced_skills`,
`crates/pi-harness/src/prompt_templates.rs::load_sourced_prompt_templates`.

**What.** The TS `loadSourcedSkills<TSource, TSkill>` has an optional
`mapSkill?: (skill, source) => TSkill` hook that lets the caller upcast/transform
each loaded skill into a richer `TSkill` type at load time. The Rust port is a
generic `load_sourced_skills<S: Clone>` that returns `SourcedSkill<S>` carrying
the plain `Skill` — the caller post-maps if needed. Same for prompt templates
(`load_sourced_prompt_templates<S>` returns `SourcedTemplate<S>` with the plain
`PromptTemplate`).

**Why.** TS variadic generics + the `TSkill extends Skill` bound don't map
cleanly to Rust. Folding a `mapSkill` callback into the loader would require a
second generic parameter `F: Fn(Skill, S) -> T` on the function, and callers
that just want identity would still pay for it. Post-mapping at the call site is
a one-line `.map(|s| SourcedSkill { skill: transform(s.skill, s.source.clone()), source: s.source })`
and is strictly more flexible.

**Resolution for review:** acceptable; the `pi-cli` (M6) and any app code can
post-map. If a hot path needs it inlined later, add an optional `mapper` param.

## 3. Symlinked skill/template directories deferred to an OS-env conformance test

**Where:** `crates/pi-harness/tests/skills_loader.rs`,
`crates/pi-harness/tests/prompt_templates.rs` (test-deferral notes).

**What.** The TS skills suite has a "loads skills through symlinked
directories" case and the prompt-templates suite has a "loads explicit markdown
files and symlinked files" case, both against `NodeExecutionEnv` + real
tempdirs + `symlink`. The `InMemoryExecutionEnv` test double does not model
symlinks faithfully: `canonical_path` is identity, and `file_info`'s `kind` is
only ever `File`/`Directory` (no `Symlink` kind is ever produced on a read path,
and `resolve_kind`'s symlink branch is unreachable for in-memory). Porting these
two cases against the in-memory env would not exercise the symlink resolution
path meaningfully.

**Resolution.** The symlink cases are deferred to a `pi-tools` OS-env conformance
test (`OsExecutionEnv` + a real tempdir), where `symlink_metadata`/`canonicalize`
are real. The in-memory integration tests cover the non-symlink shapes
(`SKILL.md` load, drop-on-missing-description, root-only `.md` children, missing
dir skipped, sourced source attachment, XML-escaped vs unescaped format paths;
template non-recursive dir load, explicit file load, sourced source, parse
diagnostic, substitution). The `resolve_kind` symlink-resolution code path in
both loaders IS ported (it canonicalizes then re-stats); it is just not
exercised by the in-memory tests. An OS-env conformance test should add a
symlink case to close the gap.

## 4. `format_skills_for_system_prompt` lives in `skills.rs`, not `system_prompt.rs`

**Where:** `crates/pi-harness/src/skills.rs::format_skills_for_system_prompt`,
`crates/pi-harness/src/system_prompt.rs`.

**What.** In the TS reference, `formatSkillsForSystemPrompt` is defined in
`system-prompt.ts`. In this port it is defined in `skills.rs` (co-located with
the loader and `format_skill_invocation`), and `system_prompt.rs` owns only the
*composition* helper `compose_system_prompt(base, skills)` that stitches the base
prompt + the listing. Each module's doc comment names this placement explicitly.

**Why.** The two format paths (invariant §9: escaped listing vs unescaped
invocation) are siblings — keeping them in one file makes the "do not unify"
invariant visually obvious and lets the inline tests assert both shapes side by
side. The composition helper is the only thing `system_prompt.rs` needs to own.
This is a cohesion choice, not a behavior change; the public API is reachable
from both `pi_harness::skills::format_skills_for_system_prompt` and (via
`compose_system_prompt`) `pi_harness::system_prompt`.

**Resolution for review:** acceptable; documented in both module doc comments.

## 5. `WatchHandle` does not auto-unsubscribe on drop (matches TS)

**Where:** `crates/pi-harness/src/events.rs::WatchHandle::drop`.

**What.** The TS `WatchHandle` does NOT remove the watch from the bus when
dropped — the caller must call `watch.unsubscribe()`. The Rust port mirrors this
exactly: `Drop for WatchHandle` is an intentional no-op (with a comment
explaining why). This means a forgotten `unsubscribe` leaks the watch (the bus
holds a strong `Arc<Mutex<WatchDelivery>>`).

**Why.** Matching TS semantics is the priority for a faithful port, and the
harness/session layer that owns `WatchHandle`s has a clear lifecycle (unsubscribe
on lane close / harness drop). Making drop auto-unsubscribe would diverge and
could surprise callers porting from TS.

**Resolution for review:** acceptable and intentional. If ergonomic pressure
appears later (e.g. `pi-cli` lane code), consider a `WatchHandle::into_unsub_on_drop()`
wrapper rather than changing the default.

## 6. `HarnessEventBus` is not `Clone`; snapshot-closure emit needs a shared-inner handle (test-only)

**Where:** `crates/pi-harness/src/events.rs` (inline `EmitterHandle` test helper),
`crates/pi-harness/tests/events_watch.rs`.

**What.** The TS `events.watch(() => { events.emit(...) })` captures `events`
directly in the snapshot closure. The Rust `HarnessEventBus` is not `Clone`
(its `Arc<Mutex<BusInner>>` is private), so the snapshot closure cannot capture a
second owned bus. The inline unit test uses a crate-private `EmitterHandle` that
shares the inner `Arc` to emit. The integration test (`events_watch.rs`) avoids
the helper: it captures a shared `&events` borrow alongside `watch`'s own
`&self` borrow — two shared borrows, which the borrow checker permits — and
emits through `&events` directly.

**Implication for M5f.** When the `AgentHarness` run loop emits `RunStart`/`RunEnd`
from inside a `watch` snapshot closure (if it ever needs to), it will either
(a) hold a shared `&HarnessEventBus` borrow (fine within a single function) or
(b) need a small crate-internal `emit` handle like the test helper. The
integration test proves (a) works for the public surface. No public API change
anticipated.

**Resolution for review:** acceptable; note for M5f implementer.

## 7. `OnUnsubscribe` is `'static` (holds a `Weak`), drop-unregisters

**Where:** `crates/pi-harness/src/events.rs::OnUnsubscribe`.

**What.** `on(...)` returns an `OnUnsubscribe` guard carrying a `Weak<Mutex<BusInner>>`
(+ type_tag + id), so it is `'static` and storable anywhere independent of the
bus borrow. `Drop` calls `off()`, removing the listener. This matches TS
`events.on(...)` returning an unsubscribe `() => void` that callers invoke (or
let drop in the Rust sense). The guard also has an explicit `off(&mut self)` for
parity with the TS "call the returned function" style.

**Resolution for review:** acceptable; faithful to TS. The `'static`-ness is a
Rust affordance that's strictly more ergonomic than the TS closure.

---

## M5e verification (2026-08-13)

```
cargo test -p pi-harness
  lib:            81 passed
  skills_loader:   6 passed
  prompt_templates:8 passed
  events_watch:    2 passed
  (+ M5b/M5c/M5d integration suites: session_state, jsonl_codec, jsonl_torn_tail,
   jsonl_atomic_publish, reducer, compaction_cut_point, compaction_summary,
   compaction_split_turn — all green)
  0 warnings
```

All M5e source modules (`frontmatter`, `skills`, `prompt_templates`,
`system_prompt`, `events`) are wired into `crates/pi-harness/src/lib.rs` and
green. Ready for M5f (AgentHarness run loop + Session/SessionTree facade).
