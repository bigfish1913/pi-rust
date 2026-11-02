# Nonexecuting saved-session resource read v1

Discover `--resource-capabilities` in bounded `--help`, then require the static
`savedSessionRead` v1 profile before invoking **only**
`rpi --resource-session-read <owner-request.json>`. No fallback probing.
The flag is intercepted before ordinary argument parsing, settings/trust,
credentials, extension/provider setup, resource resolution, stdin, or session
startup. It creates no file, lock, migration, repair, or progress checkpoint.

Request (strict UTF-8 JSON, unknown/duplicate fields rejected):
`{ "version":1, "sessionId":"…", "sessionRoot":"absolute owner root",
"sessionPath":"absolute owner-resolved JSONL", "limit":128,
"customTypes":["owner journal type"], "cursor":null }`.
`limit` is 1..128; customTypes is at most 8 unique nonempty strings, each <=256
bytes. Empty customTypes is legal; `rpi.value` is forbidden so callers cannot
request a full SessionValues/settings dump. Cursor is a strict object with version,
source fingerprint, authoritative leafId and exclusive afterSeq. Messages page
**oldest first**, only on the canonical main lane branch, through native entry
queries; no branch guessing. A cursor is tied to exact transcript AND progress
bytes and branch, not timestamps. Source change rejects the cursor.

Budgets, enforced before read/allocation where applicable:
- request 64 KiB; response 8 MiB (including envelope and newline);
- total transcript/progress/cold-range reads 64 MiB per invocation;
- history mutations <=65,536, lanes <=32;
- selected messages <=128 and <=5 MiB serialized, custom entries <=1,024
  and <=2 MiB serialized;
- cooperative processing deadline 10 seconds, checked during bounded reads,
  replay, and queries. Caller also runs a contained child with a finite deadline.
  Kernel filesystem calls are not hard-preemptible within the process.
Limits fail explicitly; no unlimited history fallback or empty success.
Historical stream frames remain cold through the canonical loader. Full bounded
history metadata is replayed because leaf, settings binding and recovery facts
must be canonical, but only selected bounded message/custom rows are cloned.

The owner resolves ID/path/root. Native canonicalizes them, requires a regular
contained transcript, validates the header ID, and takes a **noncreating shared
observation lock** on the canonical `<transcript>.lock`, held through response
preparation. Native writers hold the same file exclusively. Contention returns
Busy. Missing legacy lock returns Unsupported: deliberate ordinary open/close
is required; the reader never creates it. Symlink escapes and aliased companion
files are refused. This coordinates native writers, not hostile noncooperating
filesystem programs. Application project/session authorization remains outside.

Success returns sessionId, recorded cwd (identity, for owner project checks),
leafId, source fingerprint, bounded actual native
message page, next cursor, filtered custom entries, validated resource binding
facts (revision/fingerprint/operationId; no inventories/settings dump), and
explicit native execution/queue/recovery/progress pending facts. Legacy history
without declared native durable progress remains viewable but cannot prove idle
for replacement. Pending work is readable, never resumed or repaired here.
Malformed/torn strict history or progress returns an error without touching raw
bytes. Resource files may be missing or changed; they are never loaded.

Core must retain lifecycle/admission and independently confirm OS child exit
before offline inspection/replacement. Saved binding is a CAS expectation only;
canonical pending facts and owner journal are checked fresh on every attempt,
then native transition rechecks under its exclusive writer lock. Blocked history
uses this reader without launching an executable session generation.
