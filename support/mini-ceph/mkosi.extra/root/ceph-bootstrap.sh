#!/bin/bash

# Copyright (c) 2026 Stelia Ltd
# This file is dual-licensed under Apache 2.0 and MIT terms.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

set -euEo pipefail

vm_ip=$(ip -4 address show dev enp0s1 | sed -n -E 's/\s+inet ([0-9\.]+)\/.*$/\1/p')
cephadm bootstrap -c /etc/ceph/initial.conf --mon-ip $vm_ip --allow-overwrite
ceph orch daemon add osd fedora:/dev/nvme0n1
cat /etc/ceph/ceph.conf /etc/ceph/ceph.client.admin.keyring
