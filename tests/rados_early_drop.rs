// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Duration;

use async_ceph::{Error, rados::RadosClient, rbd::Image};

use rstest::rstest;
use serial_test::serial;

mod fixtures;
use fixtures::*;

/// Call RadosClient::shutdown while aio read ops are still in flight.
///
/// The expected behavior is that img.close() has moved ownership of the image handle
/// to the async close op, leading the RadosClient::shutdown impl to wait until the image
/// is done closing before calling `rados_shutdown`.
///
/// If this mechanism fails and `rados_shutdown` is called early, the process will abort due to some
/// asserts in librados that check if it's still being used.
#[tokio::test]
#[rstest]
#[serial]
#[timeout(Duration::from_secs(30))]
async fn in_flight_shutdown(client: RadosClient) -> Result<(), Error> {
    {
        let ctx = client.create_io_ctx("rbd").await?;
        let img = Image::open(&ctx, "sample.img").await?;

        tracing::info!("pre read ops");

        // queue a bunch of read ops, but don't await them
        for _ in 0 .. 20000 {
            drop(img.read(0, Vec::with_capacity(64)));
        }

        tracing::info!("post read ops");

        let fut = std::pin::pin!(img.close());
        let _ = futures_util::poll!(fut);
    }
    tracing::info!("waiting for in flight ops to complete before shutdown");
    client.shutdown().await;

    Ok(())
}
