// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! librados API tests.

use std::error::Error;

use async_ceph::{
    async_rt::Tokio,
    rados::{RadosClient, RadosClientBuilder},
};
use rstest::rstest;

mod fixtures;
use fixtures::*;

#[tokio::test]
#[rstest]
async fn connect_rados(_setup: ()) -> Result<(), Box<dyn Error>> {
    let _rados_client = RadosClientBuilder::default()
        .executor(&Tokio)
        .load_config_file("./ceph.conf")?
        .connect()
        .await?;

    Ok(())
}

#[tokio::test]
#[rstest]
async fn open_pool(client: RadosClient) -> Result<(), Box<dyn Error>> {
    client.create_io_ctx("rbd").await?;
    Ok(())
}
