// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::process::Command;

pub fn reset_cluster_state() {
    let mut cmd = Command::new("bash")
        .args(["support/reset-state.sh"])
        .spawn()
        .expect("failed to start state reset script");

    let exit_status = cmd.wait().expect("failed to wait for state reset script");
    assert!(exit_status.success())
}
