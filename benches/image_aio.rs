// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Duration;

use async_ceph::{rados::RadosClientBuilder, rbd::Image};

use criterion::{BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main, measurement::Measurement};

use futures_util::future::join_all;
use tokio::runtime::Runtime;

mod common;
use common::reset_cluster_state;

async fn read_small_many(image: &Image<'_>, serial: usize, concurrency: usize) {
    let futs = (0 .. concurrency).map(|_| async move {
        let mut buf = Vec::with_capacity(64);
        let mut n_read;
        for _ in 0 .. serial {
            // using a sync mutex since the lock will not be held for long
            (buf, n_read) = image.read(0, buf).await;
            n_read.unwrap();
        }
    });

    join_all(futs).await;
}

fn read_small_many_group(g: &mut BenchmarkGroup<'_, impl Measurement>, rt: &Runtime, image: Image<'_>, poll_type: &str) {
    //let image = Mutex::new(image);

    for (serial, concurrency) in [(1000, 1), (200, 5), (50, 20), (10, 100)] {
        let id = BenchmarkId::from_parameter(format!("{poll_type}, {concurrency} x ({serial} serial reads)"));

        g.bench_with_input(id, &(serial, concurrency, &image), |b, &(s, c, i)| {
            b.to_async(rt).iter(|| read_small_many(i, s, c));
        });
    }
}

pub fn criterion_benchmark(c: &mut Criterion) {
    reset_cluster_state();

    let rt = Runtime::new().unwrap();

    let rados = rt
        .block_on(
            RadosClientBuilder::default()
                .executor(&rt)
                .load_config_file("./ceph.conf")
                .unwrap()
                .connect(),
        )
        .unwrap();

    let ctx = rt.block_on(rados.create_io_ctx("rbd")).unwrap();

    let mut read_small_group = c.benchmark_group("read_small_many");
    read_small_group.measurement_time(Duration::from_secs(15));

    let image_cb = rt.block_on(Image::open(&ctx, "sample.img")).unwrap();
    read_small_many_group(&mut read_small_group, &rt, image_cb, "callback");
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
