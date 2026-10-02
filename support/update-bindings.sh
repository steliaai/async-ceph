#!/usr/bin/env bash

# Copyright (c) 2026 Stelia Ltd
# This file is dual-licensed under Apache 2.0 and MIT terms.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

set -eEuo pipefail

CEPH_GIT="https://github.com/ceph/ceph.git"
CLONE_DIR="./support/ceph-headers"

# As a sanity check, use cargo locate-project to cd into the right directory and
# check cargo pkgid

MANIFEST=$(cargo locate-project --workspace | jq -r ".root")
WORKSPACE=$(dirname "${MANIFEST}")
cd "${WORKSPACE}"

if [[ "$(cargo pkgid)" != *"#async-ceph@"* ]]; then
    printf "This script is intended to be run in the workspace of the async-ceph project.\n"
    exit 1
fi

if [[ -n "${1:-}" ]]; then
    CEPH_TAG="$1"
    printf "using requested Ceph tag: ${CEPH_TAG}\n"
else
    # First fetch latest non release candidate version tag
    # versionsort.suffix as described in `man git-ls-remote' to filter out release candidates
    # the grep is to remove annotated tag refs of the form refs/tags/TAG^{}
    CEPH_TAG=$(
        git -c 'versionsort.suffix=-' ls-remote --sort='v:refname' --tags "${CEPH_GIT}" 'refs/tags/v*' |
            grep '[^\}]$' |
            tail --lines=1 |
            cut --delimiter='/' --fields=3
    )
    printf "using most recent upstream Ceph release tag: ${CEPH_TAG}.\n"
    printf "note: if you wish to use a specific tag, pass it as a positional argument.\n"
fi

CURRENT_TAG=$(cat "${CLONE_DIR}/TAG" 2>/dev/null || true)
if [[ "${CEPH_TAG}" == "${CURRENT_TAG}" ]]; then
    printf "local ceph headers already up to date, not cloning.\n"
else
    # remove any existing files
    rm -rf "${CLONE_DIR}"

    # shallow clone that only pulls the one commit matching the tag we want
    git clone -n --depth=1 --filter=tree:0 --sparse "--revision=refs/tags/${CEPH_TAG}" "${CEPH_GIT}" "${CLONE_DIR}"
    WORK_DIR=$(pwd)
    cd "${CLONE_DIR}"

    # minimal sparse checkout for all the includes and licenses we need
    git sparse-checkout set --no-cone \
        /src/include/rados/rados_types.h \
        /src/include/rados/librados.h \
        /src/include/rbd/features.h \
        /src/include/rbd/librbd.h \
        /src/include/cephfs/libcephfs.h \
        /src/include/cephfs/ceph_ll_client.h \
        /COPYING \
        /COPYING-LGPL2.1 \
        /COPYING-LGPL3

    git checkout

    # write the cloned tag
    printf "${CEPH_TAG}" >TAG

    # git repo is no longer needed, remove it
    rm -rf .git

    cd "${WORK_DIR}"
fi

printf "(re)generating bindings...\n"

cargo clean -p rados-sys -p rbd-sys
HEADERS_PATH=$(realpath "${CLONE_DIR}/src/include")
RUSTFLAGS="--cfg ceph_sys_pregen_bindings --cfg ceph_sys_include=\"${HEADERS_PATH}\"" \
    cargo check -p rados-sys -p rbd-sys -p cephfs-sys
