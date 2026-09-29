#![allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]
use bevy_slinet::{
    bench_utils::{
        lossy_channel, ClientFixture, RawPeer, Scenario, ServerFixture, UdpClientFixture,
    },
    connection::OverflowPolicy,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::{
    hint::black_box,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

fn queues(c: &mut Criterion) {
    let mut group = c.benchmark_group("queues");
    for capacity in [1, 64, 1024] {
        group.throughput(Throughput::Elements(capacity as u64));
        let (tx, mut rx) = lossy_channel(capacity, usize::MAX, OverflowPolicy::DropNewest);
        // Warm storage, then reuse it across samples.
        for n in 0..capacity {
            tx.try_send(n, 1).unwrap();
        }
        for _ in 0..capacity {
            black_box(rx.try_recv().unwrap());
        }
        group.bench_with_input(
            BenchmarkId::new("lossy_batch", capacity),
            &capacity,
            |b, &n| {
                b.iter(|| {
                    for value in 0..n {
                        black_box(tx.try_send(black_box(value), 1).unwrap());
                    }
                    for _ in 0..n {
                        black_box(rx.try_recv().unwrap());
                    }
                });
            },
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(capacity);
        for n in 0..capacity {
            tx.try_send(n).unwrap();
        }
        for _ in 0..capacity {
            black_box(rx.try_recv().unwrap());
        }
        group.bench_with_input(
            BenchmarkId::new("tokio_batch", capacity),
            &capacity,
            |b, &n| {
                b.iter(|| {
                    for value in 0..n {
                        tx.try_send(black_box(value)).unwrap();
                    }
                    for _ in 0..n {
                        black_box(rx.try_recv().unwrap());
                    }
                });
            },
        );
        group.throughput(Throughput::Elements(1));
        for policy in [OverflowPolicy::DropNewest, OverflowPolicy::DropOldest] {
            let (tx, _rx) = lossy_channel(capacity, usize::MAX, policy);
            for n in 0..capacity {
                tx.try_send(n, 1).unwrap();
            }
            group.bench_function(
                BenchmarkId::new(format!("overflow_{policy:?}"), capacity),
                |b| {
                    b.iter(|| {
                        black_box(tx.try_send(black_box(42), 1)).ok();
                    });
                },
            );
        }
    }
    // Move the same owned allocation back and forth; no payload construction in timing.
    for size in [64, 1200] {
        let (tx, mut rx) = lossy_channel(64, usize::MAX, OverflowPolicy::DropNewest);
        let mut packet = Some(vec![1u8; size].into_boxed_slice());
        tx.try_send(packet.take().unwrap(), size).unwrap();
        packet = Some(rx.try_recv().unwrap());
        group.bench_function(BenchmarkId::new("owned_payload", size), |b| {
            b.iter(|| {
                tx.try_send(black_box(packet.take().unwrap()), size)
                    .unwrap();
                packet = Some(black_box(rx.try_recv().unwrap()));
            });
        });
    }
    group.finish();
}

// Persistent producer threads and runtimes. A barrier starts a fixed batch each
// iteration; timings include rendezvous and wakeups, but not thread creation.
fn concurrent(c: &mut Criterion) {
    const ITEMS: usize = 4096;
    let mut group = c.benchmark_group("handoff");
    group.throughput(Throughput::Elements(ITEMS as u64));
    for producers in [1, 4] {
        for capacity in [1, 64] {
            for lossy in [true, false] {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                let barrier = Arc::new(Barrier::new(producers + 1));
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (ltx, mut lrx) =
                    lossy_channel(capacity, usize::MAX, OverflowPolicy::DropNewest);
                let (ttx, mut trx) = tokio::sync::mpsc::channel(capacity);
                let threads: Vec<_> = (0..producers)
                    .map(|producer| {
                        let barrier = barrier.clone();
                        let stop = stop.clone();
                        let ltx = ltx.clone();
                        let ttx = ttx.clone();
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .build()
                                .unwrap();
                            loop {
                                barrier.wait();
                                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                                    break;
                                }
                                rt.block_on(async {
                                    for n in 0..ITEMS / producers {
                                        let value = producer * (ITEMS / producers) + n;
                                        if lossy {
                                            ltx.send(value, 1).await.unwrap();
                                        } else {
                                            ttx.send(value).await.unwrap();
                                        }
                                    }
                                });
                            }
                        })
                    })
                    .collect();
                let name = format!(
                    "{}/{producers}p/{capacity}",
                    if lossy { "lossy_wait" } else { "tokio_wait" }
                );
                group.bench_function(name, |b| {
                    b.iter(|| {
                        barrier.wait();
                        let sum = runtime.block_on(async {
                            let mut sum = 0;
                            for _ in 0..ITEMS {
                                sum += if lossy {
                                    lrx.recv().await.unwrap()
                                } else {
                                    trx.recv().await.unwrap()
                                };
                            }
                            sum
                        });
                        assert_eq!(sum, ITEMS * (ITEMS - 1) / 2);
                        black_box(sum);
                    });
                });
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                barrier.wait();
                for thread in threads {
                    thread.join().unwrap();
                }
            }
        }
    }
    group.finish();
}

fn ecs(c: &mut Criterion) {
    let mut group = c.benchmark_group("ecs");
    group.throughput(Throughput::Elements(128));
    for budget in [1, 16, 256] {
        for scenario in [Scenario::Packets, Scenario::Interleaved] {
            // 32 peers x 4 packets: interleaved lifecycle boundaries exercise HOL blocking.
            macro_rules! measure {
                ($fixture:ty, $side:literal) => {{
                    let mut fixture = <$fixture>::new(budget, 32, 4, scenario);
                    fixture.enqueue();
                    let delivery = fixture.drain();
                    println!("delivery/{}/{scenario:?}/{budget}: {delivery:?}", $side);
                    group.bench_function(format!("{}/{scenario:?}/{budget}", $side), |b| {
                        b.iter_custom(|iters| {
                            let mut elapsed = Duration::ZERO;
                            for _ in 0..iters {
                                fixture.enqueue();
                                let started = Instant::now();
                                black_box(fixture.drain());
                                elapsed += started.elapsed();
                            }
                            elapsed
                        });
                    });
                }};
            }
            measure!(ClientFixture, "client");
            measure!(ServerFixture, "server");
        }
    }
    // Existing UDP peers: finish only when gameplay in Update sees all packets.
    for budget in [16, 256] {
        let mut fixture = UdpClientFixture::udp(budget, 32, 4);
        fixture.enqueue();
        println!(
            "delivery/udp_client_to_update/{budget}: {:?}",
            fixture.drain()
        );
        group.bench_function(format!("udp_client_to_update/{budget}"), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    fixture.enqueue();
                    let started = Instant::now();
                    black_box(fixture.drain());
                    elapsed += started.elapsed();
                }
                elapsed
            });
        });
    }
    group.finish();
}
fn udp(c: &mut Criterion) {
    let mut peer = RawPeer::new();
    let mut group = c.benchmark_group("udp_dispatch");
    for size in [0, 64, 1200] {
        let bytes = vec![7; size];
        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("existing_peer", size), |b| {
            b.iter(|| {
                assert_eq!(peer.roundtrip(black_box(&bytes)), size);
            });
        });
        group.bench_function(BenchmarkId::new("create_drop_peer", size), |b| {
            b.iter(|| peer.create_and_drop_peer(black_box(&bytes)));
        });
    }
    group.finish();
}
criterion_group! { name = benches; config = Criterion::default().sample_size(20).warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_secs(1)); targets = queues, concurrent, ecs, udp }
criterion_main!(benches);
