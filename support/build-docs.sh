#!/usr/bin/env bash

# Copyright (c) 2026 Stelia Ltd
# This file is dual-licensed under Apache 2.0 and MIT terms.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

set -euEo pipefail

RUSTFLAGS="${RUSTFLAGS:-} --cfg ceph_sys_bundled" RUSTDOCFLAGS="${RUSTDOCFLAGS:-} --cfg docsrs" cargo +nightly doc --no-deps --workspace --all-features
