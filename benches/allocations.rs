#![allow(clippy::unwrap_used, clippy::cast_precision_loss)]
use bevy_slinet::{
    bench_support::{lossy_channel, ClientFixture, RawPeer, Scenario, ServerFixture},
    connection::OverflowPolicy,
};
use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};
use std::{alloc::System, hint::black_box};
#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;
const ITEMS: usize = 1024;
fn measure(name: &str, items: usize, run: impl FnOnce()) {
    let region = Region::new(GLOBAL);
    run();
    let stats = region.change();
    let n = items as f64;
    println!(
        "{name},{items},{:.4},{:.4},{:.4},{:.4}",
        stats.allocations as f64 / n,
        stats.reallocations as f64 / n,
        stats.bytes_allocated as f64 / n,
        stats.deallocations as f64 / n
    );
}
fn main() {
    println!("scenario,items,allocations/item,reallocations/item,allocated_bytes/item,deallocations/item");
    // Validate that the counter sees a known live allocation before trusting results.
    let region = Region::new(GLOBAL);
    let calibration = black_box(vec![0u8; 4096]);
    assert_eq!(region.change().allocations, 1);
    assert_eq!(region.change().bytes_allocated, 4096);
    drop(calibration);
    for policy in [OverflowPolicy::DropNewest, OverflowPolicy::DropOldest] {
        let (tx, mut rx) = lossy_channel(64, usize::MAX, policy);
        let name = format!("queue/{policy:?}/cold_fill");
        measure(&name, 64, || {
            for n in 0..64 {
                black_box(tx.try_send(n, 1).unwrap());
            }
        });
        let name = format!("queue/{policy:?}/overflow");
        measure(&name, ITEMS, || {
            for n in 0..ITEMS {
                black_box(tx.try_send(n, 1)).ok();
            }
        });
        for _ in 0..64 {
            black_box(rx.try_recv().unwrap());
        }
        let name = format!("queue/{policy:?}/warm_roundtrip");
        measure(&name, ITEMS, || {
            for n in 0..ITEMS {
                tx.try_send(n, 1).unwrap();
                black_box(rx.try_recv().unwrap());
            }
        });
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    // Warm Tokio blocks with the same occupancy as the timed path.
    for n in 0..ITEMS {
        tx.try_send(n).unwrap();
        black_box(rx.try_recv().unwrap());
    }
    measure("tokio/warm_roundtrip", ITEMS, || {
        for n in 0..ITEMS {
            tx.try_send(n).unwrap();
            black_box(rx.try_recv().unwrap());
        }
    });
    let mut peer = RawPeer::new();
    for size in [0, 64, 1200] {
        let bytes = vec![1; size];
        peer.roundtrip(&bytes);
        let name = format!("udp/existing_peer/{size}");
        measure(&name, ITEMS, || {
            for _ in 0..ITEMS {
                assert_eq!(peer.roundtrip(&bytes), size);
            }
        });
        peer.create_and_drop_peer(&bytes);
        let name = format!("udp/create_drop_peer/{size}");
        measure(&name, ITEMS, || {
            for _ in 0..ITEMS {
                peer.create_and_drop_peer(&bytes);
            }
        });
    }
    drop(peer);
    for scenario in [Scenario::Packets, Scenario::Interleaved] {
        macro_rules! measure_ecs {
            ($fixture:ty, $side:literal) => {{
                let mut fixture = <$fixture>::new(256, 32, 4, scenario);
                fixture.enqueue();
                fixture.drain();
                fixture.enqueue();
                let name = format!("ecs/{}/{scenario:?}", $side);
                measure(&name, 128, || {
                    black_box(fixture.drain());
                });
            }};
        }
        measure_ecs!(ClientFixture, "client");
        measure_ecs!(ServerFixture, "server");
    }
}
