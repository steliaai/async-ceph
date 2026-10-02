// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::process::Command;

use async_ceph::{
    async_rt::Tokio,
    rados::{IoCtx, RadosClient, RadosClientBuilder},
};
use rstest::fixture;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{Layer, filter, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt};

fn reset_cluster_state() {
    let mut cmd = Command::new("bash")
        .args(["support/reset-state.sh"])
        .spawn()
        .expect("failed to start state reset script");

    let exit_status = cmd.wait().expect("failed to wait for state reset script");
    assert!(exit_status.success())
}

#[fixture]
#[once]
pub fn init_trace() {
    let filter_async_ceph = filter::Targets::new().with_target("async_ceph", LevelFilter::DEBUG);
    let filter_other = filter::Targets::new().with_default(LevelFilter::INFO);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_thread_ids(true)
                .with_test_writer()
                .with_filter(filter_async_ceph),
        )
        .with(tracing_subscriber::fmt::layer().with_thread_ids(true).with_filter(filter_other))
        .init();
}

#[fixture]
#[once]
pub fn setup(_init_trace: ()) {
    reset_cluster_state();
}

#[fixture]
pub fn client(_setup: ()) -> RadosClient<'static> {
    let connect_fut = RadosClientBuilder::default()
        .executor(&Tokio)
        .load_config_file("./ceph.conf")
        .expect("failed to load config")
        .connect();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(connect_fut)
        .expect("failed to connect to cluster")
}

/// When a test fails by panicking, resets the cluster state so the next test can run as expected
///
/// This should be added as an argument to the tests
pub struct CleanupOnPanic;
impl Drop for CleanupOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::warn!("test failed, resetting cluster state");
            reset_cluster_state();
        }
    }
}

/// Error type that resets the cluster state when constructed.
///
/// This can be used as the error type in a `Result` returning test. Note
/// that if the test can also panic, it should be used with and not replace the
/// `cleanup` fixture.
#[derive(Debug)]
#[allow(unused)]
pub struct CleanupOnError(Box<dyn std::error::Error>);

impl std::fmt::Display for CleanupOnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl<T: std::error::Error + 'static> From<T> for CleanupOnError {
    fn from(value: T) -> Self {
        tracing::warn!("test failed, resetting cluster state");
        reset_cluster_state();
        Self(Box::new(value))
    }
}

#[fixture]
pub fn cleanup() -> CleanupOnPanic {
    CleanupOnPanic
}

// this is used in rbd tests, but clippy sees it's not used in rados tests
#[allow(unused)]
#[fixture]
pub fn ctx(client: RadosClient<'static>) -> IoCtx<'static> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(client.create_io_ctx("rbd"))
        .expect("failed to create ioctx")
}
