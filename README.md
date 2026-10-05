# async-ceph

![Crates.io Version](https://img.shields.io/crates/v/async-ceph)
![Crates.io MSRV](https://img.shields.io/crates/msrv/async-ceph)
![docs.rs](https://img.shields.io/docsrs/async-ceph)
![GitHub checks CI](https://img.shields.io/github/check-runs/steliaai/async-ceph/main?nameFilter=Checks%20CI%20is%20green&label=checks)
![GitHub build CI](https://img.shields.io/github/check-runs/steliaai/async-ceph/main?nameFilter=Build%20CI%20is%20green&label=build)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/steliaai/async-ceph/badge)](https://scorecard.dev/viewer/?uri=github.com/steliaai/async-ceph)

High level Async Rust bindings to librados, librbd and libcephfs.

While these libraries expose callback-based asynchronous APIs for a small subset of their functionality (mostly IO), the bulk of it is only available through blocking APIs. If you need to be careful not to block the executor, this has either leads to significant boilerplate through repeated `spawn_blocking` calls, or having to design the application such that RADOS/RBD blocking logic is isolated from the application's async code.

This crate aims to solve this burden while remaining runtime agnostic by abstracting over async executors that provides an IO threadpool through the `async_ceph::async_rt::Executor` trait. After an `Executor` implementation is provided to the `RadosClient` constructor, traditionally blocking librados/librbd/libcephfs operations can be seamlessly called from an `async` context.

### Naming

Note that this crate may be confused with `ceph-async`. It is an unaffiliated project by an adding async APIs for AIO operations to the official synchronous RADOS bindings. `async-ceph` (this crate) is instead designed to be ergonomically used within `async` contexts alone, and focuses on block device (RBD) APIs.

## RADOS and RBD API Support

Since this crate was built for Stelia's use cases, which mainly revolve around RBD functionality, RADOS support is currently limited to APIs that are required to initialize a client (`rados_t`) and create an IO context (`rados_ioctx_t`). CephFS support is also fairly limited, and currently allows statting and setting extended attributes on files. RBD support (gated behind the `rbd` feature), while not comprehensive, is sufficient for common usage:

- [x] Image options APIs
- [x] Image features APIs
- [x] Image listing, creation, cloning and deletion
- [x] Fetching various image metadata (features, size, timestamps, parents, descendants, group, etc)
- [x] Snapshot APIs (listing, creation, removal, fetching metadata, etc.)
- [x] Advisory locking APIs
- [x] Async IO and vectored IO APIs (read, write, discard, compare-and-write, zeroing)
- [x] RBD update watches
- [x] Diff iteration (`rbd_diff_iterate2/3`)
- [ ] Read iteration (`rbd_read_iterate2`)
- [ ] Image deep copying (`rbd_copy_*` functions)
- [ ] Fine-grained (non-advisory) locking APIs
- [ ] RBD migration APIs
- [ ] RBD trash APIs
- [ ] RBD mirroring APIs
- [ ] RBD group APIs
- [ ] RBD namespace APIs
- [ ] Pool stats
- [ ] Quiesce watches

Note that the above **is not a roadmap**.

### Project Goals & Sustainability

This open-source release aims to promote industry standardization and ecosystem adoption for asynchronous Rust applications built on Ceph.
The `@steliaai/platform-engineering` team is committed to actively maintaining this project. We guarantee designated internal maintainers will triage issues, review pull requests, and publish releases for at least **12 months** from the initial public release. While we will prioritize the implementation of APIs required for our internal use cases, external contributions are always welcome.

### RBD API Support

When the `rbd` feature is enabled, this crate has a minimum supported librbd version of 1.17.0. This limits the available API to be compatible with the librbd 1.17.0 headers and shared library. To use more recent features, several version feature flags of the form `rbd_v{major}-{minor}` are available, as well as fine-grained support feature flags `rbd_{feature name}` that enable APIs gated by `LIBRBD_SUPPORTS_{FEATURE NAME}` defines in the C headers. For more information see the crate's `Cargo.toml` manifest and the `rbd-sys` documentation.

### Minimum Supported Rust Version (MSRV) Policy

The MSRV is currently Rust 1.88. It is not part of our SemVer guarantees and we may increment it as part of minor version updates.

## Usage

Run the following command to add `async-ceph` to your dependencies:

```sh
cargo add async-ceph -F rbd
```

If you are using `Tokio`, add the `tokio_rt` feature to access `Executor` implementations that
defer to the current `Runtime`:

```sh
cargo add async-ceph -F rbd,tokio_rt
```

<div class="warning">

Note that `async-ceph` depends on the `rados-sys` crate, which by default expects headers from the `librados-devel` Fedora package (naming may differ on other distributions) to be available on the system when built. Likewise, when the `rbd` feature is enabled `async-ceph` depends on `rbd-sys` which has expects headers from `librbd-devel` to be available. See the [build configuration](#build-configuration) section for more information.

</div>

The crate may then be used to, for example, asynchronously read from a RBD image as follows:

```rust,no_run
use async_ceph::{rados::RadosClient};

// This example requires the `tokio_rt` feature to provide an `Executor` implementation
// the crate can use to spawn tasks with. This trait can be implemented manually if you wish.
#[cfg(not(all(feature = "tokio_rt", feature = "rbd")))]
fn main() {}

#[tokio::main]
#[cfg(all(feature = "tokio_rt", feature = "rbd"))]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use async_ceph::{rbd::Image, async_rt::Tokio};

    // Connect using `/etc/ceph/ceph.conf`
    // Use the RadosClientBuilder for more complex configurations.
    let rados = RadosClient::new(&Tokio).await?;
    let io_ctx = rados.create_io_ctx("rbd").await?;
    let image = Image::open(&io_ctx, "my-image").await?;
    // image.read can use the uninitialized bytes of the Vec!
    // If you want to avoid this, pass `buf.slice(..buf.len())` instead.
    let buf = Vec::with_capacity(1024);
    // The buffer must be passed by value since the async op may outlive the future.
    let (buf, res) = image.read(0, buf).await;
    let len = res?;

    Ok(())
}
```

## Buffer-consuming async operations

Async operations on images that take a data buffer must take this buffer by value, since it must live
until the underlying librbd operation is done using it. It can then be reclaimed through the return value,
as shown in the above code snippet.

However, when doing reads/write inside a loop, this pattern does not work. To get around this, you must
also define the result variable as mutable *outside* the loop:

```rust,ignore
let mut buf = Vec::with_capacity(64);
let mut res; // <-- Define the variable taking the result part of `read()` here
for _ in 0 .. count {
    // This way you don't need to use any `let` here and can assign
    // both variables through a tuple
    (buf, res) = image.read(0, buf).await;
    let _n_read = res?;
}
```

## Building

`async_ceph` is designed to support a wide set of librados and librbd versions. As such, by default its system libraries `rados-sys` and `rbd-sys` are built from the system include paths using `bindgen` and also link to their respective shared libraries by letting the linker find them in system shared library paths.

This means that for the crate to build with its default configuration, on Debian you will need the following packages to be installed on your system:

- `librados-dev`
- `librbd-dev` (when the `rbd` feature is enabled)
- `libclang-**-dev` (for `bindgen`)

On alpine, you will need

- `ceph**-dev`
- `linux-headers` (used by `librados`)
- `clang**-libclang` (for `bindgen`)

However, it's possible to get away with less by passing custom `cfg` directives to rustc as explained below.

## Build Configuration

### Headers

By default, `rbd-sys` and `rados-sys` will attempt to find the librbd C headers in the system include paths. This behavior can be overridden in two mutually incompatible ways:
- By passing `--cfg ceph_sys_bundled` to rustc. This will use pregenerated bindings fetched from the latest stable Ceph release at the time the crate was released (currently 20.2.4). This option removes the need for `libclang` and `linux-headers` packages.
- By passing `--cfg ceph_sys_include=/path/to/ceph/src/include` to rustc, where `include` is a folder that contains the librbd, rados and cephfs includes under `include/rbd`, `include/rados` and `include/cephfs`, respectively. **This path must be absolute**.

Using any of these cfgs will also disable automatic dynamic linking.

### Linking

By default, `rbd-sys` and `rados-sys` will emit `cargo:rustc-link-lib` instructions to link to their respective shared libraries. This works well with devel Ceph packages that install `librbd.so` symlinks, but typically library-only packages instead have it with a version postfix. You can disable this automatic linking by passing `--cfg ceph_sys_nolink` to rustc and linking to the exact desired version yourself.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Licensing

The `async-ceph` crate contains code from the [`tokio-uring`](https://github.com/tokio-rs/tokio-uring) project, with minor modifications, licensed under MIT:

```txt
Copyright (c) 2021 Carl Lerche

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

For concurrency tests, we also adapt the `oneshot` channel from the [`futures`](https://github.com/rust-lang/futures-rs) project to use Loom concurrency primitives. This code is licensed under MIT:

```txt
Copyright (c) 2016 Alex Crichton
Copyright (c) 2017 The Tokio Authors

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

The `support/ceph-headers` directory in this repository contains header files from the [Ceph](https://github.com/ceph/ceph) project, version 20.2.4. These are licensed under the GNU Lesser General Public License, version 2.1 or 3.0 (LGPL-2.1 OR LGPL-3.0), with license files `support/ceph-headers/COPYING`, `support/ceph-headers/COPYING-LGPL2.1` and `support/ceph-headers/COPYING-LGPL3`. The full library can be obtained at <https://github.com/ceph/ceph/tree/v20.2.4>. 

Additionally, the pre-generated `bindings.rs` we provide with `-sys` crates are generated from the aforementioned Ceph header files, and thus also licensed under the LGPL.

Note that the `async-ceph` binaries generated by this project dynamically link to the underlying Ceph C libraries (`librados` and `librbd`) at runtime. This dynamic linkage ensures a clean separation between the permissive outbound license of this Rust crate and the copyleft LGPL-3.0 licenses of the Ceph libraries.

## Trademarks

Ceph is a registered trademark of the Linux Foundation. This project is not affiliated with, endorsed by, or sponsored by the Linux Foundation or the Ceph project. The `async-ceph` namespace is used strictly in a descriptive sense to indicate that this project provides Rust bindings for Ceph.
