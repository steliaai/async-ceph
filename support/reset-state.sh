#!/usr/bin/env bash

# Copyright (c) 2026 Stelia Ltd
# This file is dual-licensed under Apache 2.0 and MIT terms.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

set -euEo pipefail

function cleanup() {
    ceph -c ceph.conf osd pool delete rbd rbd --yes-i-really-really-mean-it
}

printf "\nresetting test cluster state\n\n"

ceph -c ceph.conf osd pool delete rbd rbd --yes-i-really-really-mean-it

trap cleanup ERR

ceph -c ceph.conf osd pool create rbd 1 1 replicated
ceph -c ceph.conf osd pool set rbd min_size 1
ceph -c ceph.conf osd pool set rbd size 1 --yes-i-really-mean-it

rbd -c ceph.conf pool init rbd
rbd -c ceph.conf import tests/sample.img

printf "cluster ready for tests\n\n"
