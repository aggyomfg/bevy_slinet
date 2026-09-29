# Networking benchmarks

These benches measure the current queue and event-delivery implementation. They
are a baseline for future changes, not a comparison against the pre-0.19 library.
Run on an otherwise idle machine in the optimized bench profile:

```sh
cargo bench --features bench-internals --bench network -- --noplot
cargo bench --features bench-internals --bench allocations
```

`network` uses Criterion; `allocations` is a separate CSV-producing executable
with an instrumented system allocator. Keeping them separate avoids charging
allocation-counter atomics to the timing results. `bench-internals` enables
hidden, unsupported fixtures; normal builds do not compile them. It implies the
client/server, TCP/UDP and bitcode features needed by these fixtures.

For a quick execution check, without statistical measurements:

```sh
cargo bench --features bench-internals --bench network -- --test
cargo test --features bench-internals frame_budget_and_phase_boundaries_are_visible
```

The handoff scenarios intentionally await real channel progress. On Linux, use
an outer watchdog such as `timeout 240s cargo bench ...` in automation so a wakeup
regression cannot hang the job indefinitely. Do not run tests or other builds in
parallel with recorded timing runs.

## Scenarios and boundaries

| Group | Work measured | Preparation excluded |
|---|---|---|
| `queues/*_batch/{capacity}` | Fill and drain a warmed queue of `usize` values, capacities 1/64/1024; lossy queue vs bounded Tokio MPSC | Channel construction and first storage growth |
| `queues/overflow_*` | One send to a full queue, including disposal of evicted values | Prefilling |
| `queues/owned_payload` | Enqueue/dequeue ownership of the same 64/1200-byte box | Payload allocation; no copying here |
| `handoff/*` | 4096 messages, 1/4 producer threads and one consumer, capacity 1/64 | Thread/runtime creation |
| `ecs/*` | Drain 128 packets from 32 peers through actual client/server TCP lifecycle and packet ECS systems | App creation, schedule initialization, enqueueing, one warmup drain |
| `udp_dispatch/existing_peer` | Server dispatch, raw queue, runtime entry and packet receive with a length-only decoder | Socket creation, payload construction and peer creation |
| `udp_dispatch/create_drop_peer` | Dispatch first datagram, create and drop an unsplit server peer | Listener/socket construction |

Handoff uses waiting sends for **both** channel implementations. There are no
intentional drops; the received count and checksum must match the submitted
batch. Timing includes the batch-start barrier, contention, wakes and runtime
scheduling. It is a batch throughput measurement, not per-message tail latency.

Queue throughput counts a complete enqueue/dequeue pair as one item. Batch time
is for the entire capacity, not a single item. Overflow benchmarks count each
attempted send, including rejected sends. `DropOldest` and `DropNewest` implement
different policies and must not be treated as interchangeable algorithms.

UDP dispatch is injected directly into the production listener: no datagram is
sent through the OS socket. A loopback socket is bound during setup because the
listener owns it. The decoder returns the payload length without allocating, so
codec output allocations are excluded. This path does include `async_trait`
packet-reader futures. This suite does not measure UDP ECS publication, client
socket receive, outgoing serialization, pacing or network delivery latency.

## Frame latency versus execution time

ECS fixtures enqueue either just packets from already published peers, or
`Established, Packet x4, Closed` for each of 32 peers. Budgets are 1/16/256.
The fixtures install the production systems in their actual schedules (client
lifecycle in `PreUpdate`, packets in `PostUpdate`; server both in `PreUpdate`
with lifecycle first). They do not start networking threads.

Before timing each scenario, the bench prints a `Delivery` record with total
frames, packet/establishment/closure counts and frames of the last packet and
closure. Enqueueing is outside the timer; draining includes observer calls and
small counters. Every run has a finite frame limit and must deliver the expected
number of packets and closure events. Unit tests verify the frame budget,
phase-boundary delay and reuse of each fixture.

The app is advanced without real-time sleeps. A fast CPU can execute many
`App::update()` calls in a millisecond; that does not eliminate the corresponding
frame delay in a game. Convert measured frame counts using the application's
actual frame duration. The first preloaded packet can be delivered in frame 1;
frames-to-drain and additional frames of waiting are different quantities.

## Allocation report

The CSV reports allocations, reallocations, allocated bytes and deallocations
per item. For ECS, the denominator is the 128 packet events; interleaved results
also include processing 32 establishment and 32 closure events. Allocated bytes
are allocator-requested bytes including positive realloc growth, **not** peak
resident memory or allocator metadata. Counter calibration checks a known
4096-byte allocation first. Reporting and preparation run outside each region.

Queue cold fill and warmed operations are separate. Peer creation/drop includes
cleanup; existing-peer dispatch reuses its storage. ECS measures a warmed drain
only: allocations made when creating connections or enqueueing their events are
excluded. Deallocations may therefore belong to objects allocated before the
measurement region. Allocation results are serial, without network workers.

## Comparing changes

```sh
cargo bench --features bench-internals --bench network -- --noplot --save-baseline before
# Make the candidate change, using the same compiler, machine and workload.
cargo bench --features bench-internals --bench network -- --noplot --baseline before
```

Filter a group by adding `queues`, `handoff`, `ecs` or `udp_dispatch` after `--`.
The default is a short local run: 20 samples, 300 ms warmup and 1 s measurement
per case. For decisions based on small timing differences, repeat with longer
windows, for example `--sample-size 50 --warm-up-time 2 --measurement-time 5`.
Criterion stores results under the configured Cargo target directory's
`criterion/` directory. Avoid fixed wall-time pass/fail thresholds on shared CI
runners; execution checks and frame-count tests are suitable there.

See [Criterion timing loops](https://criterion-rs.github.io/book/user_guide/timing_loops.html)
and [stats_alloc](https://docs.rs/stats_alloc/0.1.10/stats_alloc/) for measurement
mechanics. Local measurements in `docs/benchmark-results/` and machine-specific
reports such as `docs/benchmark-baseline.md` are ignored by Git.
