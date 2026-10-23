# Provider resource reuse measurements

An opt-in provider context owns a lazily initialized one-worker runtime and a
bounded cache of credential-free HTTP clients. Request threads drive synchronous
emit callbacks; independent requests do not hold a global execution mutex.
Credentials remain request-specific. Hosts without lifecycle registration use
request-scoped resources instead.

The plugin lifecycle callbacks reject/cancel work at begin-shutdown and join
workers at finish-shutdown. The host keeps the library and callback context alive
through outstanding FFI calls and string frees. Shutdown failure retains them;
a timeout must never unload code that could still execute.

Three release runs exercised the actual context driver, client construction and
a deterministic account echo without external network I/O. Each scenario made
200 requests. Scoped resources constructed 200 runtimes and 200 clients; shared
resources constructed one of each, sequentially and with four concurrent
accounts. Each request checked its own account result. Separate registration
instances and actual HTTP connection reuse with distinct Authorization headers
have separate loopback tests.

These are historical results before rebasing, retained on the fork's
`bench/provider-series-measured` branch. Environment and timing caveats are in
[the snapshot report](provider-snapshots.md).

| Scenario | Scoped / shared median ms | Scoped / shared range ms |
|---|---:|---:|
| Sequential | 23.034 / 15.873 | 23.028–25.879 / 15.280–16.629 |
| Four accounts | 7.475 / 7.092 | 7.274–7.767 / 6.531–7.379 |

Concurrent ranges overlap. Whole-process CPU was 0.09–0.10 s and peak RSS was
6,236–6,520 KiB, covering all four scenarios together. No allocation instrumentation
was used in this fixture. Resource reuse claims rely on creation counts and the
connection tests, not timing.

```bash
cargo test --release -p rpi-plugin-sdk --features provider benchmark_context_resources -- --ignored --nocapture
```

Raw samples: [provider resources](results/provider-resources.txt).
