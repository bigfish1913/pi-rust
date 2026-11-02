# Native fork resource initialization

Fork scope and executable configuration are distinct:

* An omitted branch target means the **actual main-lane leaf**, including a
  trailing custom/resource binding entry. Copy its entire ordinary branch; do
  not find a message ancestor. Explicit targets remain message-only, including
  the existing Before/At defaults and validation. Tree scope remains tree scope.
* Native host forks/clones inherit the source's **current** exact declaration
  and validated inventory, independently of an explicit historical message cut.
  They do not reactivate an older package from historical branch entries.
* Every configured destination gets a native-owned initial binding: revision 1,
  current declaration/inventory, no operationId and no operation retry ledger.
  Copied parent resource entries remain historical records, not the new session's
  current authority. Operation identities belong to their session; reusing a
  textual parent operation ID with a new destination CAS is not a parent retry.
* The normal repository generates a fresh session ID and retains parentSessionId,
  copied entry IDs/history, labels and names. Ordinary exact reopen validates the
  new binding/inventory as usual. No source bytes, leaf, binding, or operation
  ledger are changed, and no source progress is transferred as pending work.
* Legacy sources without an exact binding receive no new resource value and do
  not opt in implicitly. Existing ordinary resource startup/reopen rules remain.

## Mechanism and ownership

`SessionState::create_fork_mutations` is still the only fork history engine. Its
implicit-leaf case accepts nonmessage entries; explicit-target validation is
unchanged. `JsonlSessionStorage::fork_with_value_initializer` and the corresponding
repository seam stage new-identity values in the SAME normal atomic destination
publication. The callback sees source-wide SessionValues from the exact source
snapshot used to produce the fork, not a separately reopened or branch-reduced
view. Destination entries are validated before source-independent publication.
An initialization error does not publish a destination. Source snapshot loading
uses ordinary cold replay without repairing/writing the original source.

The private CLI resource initializer validates the current persisted binding,
extracts only declaration/inventory and creates the bounded revision-1 value.
Both startup `--fork` and in-process native Fork actions use it. This does not
introduce a second fork engine or a public append route for resource authority;
existing RPC/plugin generic appends to reserved resource values remain forbidden.
The new value is staged before publication, not appended as a correction after a
potentially authoritative inherited ledger has already been published.

## Regression coverage

* Real disk native host initializer test: current custom leaf and explicit old
  message At cut, current rather than historical declaration, copied history IDs,
  fresh revision one without source operations, same textual operation accepted
  on a new-session CAS chain, invalid explicit custom target and initializer
  rejection publish nothing, source bytes unchanged.
* Real native processes/loopback messages: Package A -> B leaves a trailing B
  custom binding; current clone and native historical navigation retain current
  Package B, fresh host/session identity, parent/history, no current parent ledger,
  ordinary B reopen succeeds and A reopen is rejected. The parent operation ID
  can then start an independent B -> A destination transition and reopen correctly;
  the original source remains byte-for-byte unchanged.
* Real legacy native fork with a custom-only leaf retains its history without
  opting into exact resources; ordinary legacy reopen succeeds.
* Existing native fork, progress omission and atomic-publication failure tests
  remain in force. Core integration exercises current fork, clone, historical
  message fork, settings consumption/source capture, and ordinary configured reopen.
