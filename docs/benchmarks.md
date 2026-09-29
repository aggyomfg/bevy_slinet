# Benchmarks

```sh
cargo bench --features bench-internals --bench network -- --noplot
cargo bench --features bench-internals --bench allocations
```

[network.rs](../benches/network.rs) measures time with Criterion;
[allocations.rs](../benches/allocations.rs) reports allocator counts as CSV.
`bench-internals` enables unsupported internal fixtures.

These are microbenchmarks: UDP dispatch bypasses OS receive; ECS drains preloaded
events. They do not measure end-to-end delivery. Frame counts and CPU execution
time are separate metrics. Allocated bytes are not peak memory usage.
`ecs/udp_client_to_update/{16,256}` drains 128 preloaded UDP packets across 32
established peers through the production client receive systems and observers,
until an `Update` system sees all packets. Enqueueing, connection establishment
and schedule initialization are outside the measured region. Moving receive back
to `PostUpdate` adds one delivery frame, visible in the printed `Delivery` record.
The allocations bench includes the same workload with budget 256. This does not
measure socket I/O, startup cost or first-peer establishment.

Run just this scenario with:

```sh
cargo bench --features bench-internals --bench network -- udp_client_to_update --noplot
```

See the benchmark code for other workloads and measurement boundaries.

Compare on the same idle machine and compiler:

```sh
cargo bench --features bench-internals --bench network -- --noplot --save-baseline before
# Apply the change.
cargo bench --features bench-internals --bench network -- --noplot --baseline before
```

Use `--test` instead of `--noplot` for an execution check. Put an external timeout
around automated runs: handoff cases wait for channel progress. Local results in
`docs/benchmark-results/` and `docs/benchmark-baseline.md` are ignored by Git.
