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
See the benchmark code for workloads and measurement boundaries.

Compare on the same idle machine and compiler:

```sh
cargo bench --features bench-internals --bench network -- --noplot --save-baseline before
# Apply the change.
cargo bench --features bench-internals --bench network -- --noplot --baseline before
```

Use `--test` instead of `--noplot` for an execution check. Put an external timeout
around automated runs: handoff cases wait for channel progress. Local results in
`docs/benchmark-results/` and `docs/benchmark-baseline.md` are ignored by Git.
