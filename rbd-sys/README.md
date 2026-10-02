# rbd-sys

Low-level bindings to Ceph's `librbd`.

# Building

`rbd-sys` is designed to support a wide set of librbd versions, and as such relies on `bindgen` to generate its bindings from the librbd headers available on the host system. By default it pulls these headers from system include paths, and links to `librbd.so` in system shared library paths.

This means that for the crate to build with its default configuration, on Debian you will need the following packages to be installed on your system:

- `librados-dev`
- `librbd-dev`
- `libclang-**-dev` (for `bindgen`)

On alpine, you will need

- `ceph**-dev`
- `linux-headers` (used by `librados`)
- `clang**-libclang` (for `bindgen`)

However, it's possible to get away with less by passing custom `cfg` directives to rustc as explained below.

# Configuration

## Headers

By default, `rbd-sys` will attempt to find the librbd C headers in the system include paths. This behavior can be overridden in two mutually incompatible ways:
- By passing `--cfg ceph_sys_bundled` to rustc. This will use pregenerated bindings fetched from the latest stable Ceph release at the time the crate was released (currently 20.2.4). This option removes the need for `libclang` and `linux-headers` packages.
- By passing `--cfg ceph_sys_include=/asbolute/path/to/ceph/src/include` to rustc, where `include/rbd` is a folder that contains the librbd include files. **This path must be absolute**. Note that `librbd.h` includes `../rados/librados.h`, so you should also have the appropriate librados includes in `/include/rados`.

Using any of these cfgs will also disable linking to `librbd.so` in system paths, as documented below.

## Linking

By default, `rbd-sys` will emit `cargo:rustc-link-lib=rbd` to dynamically link to `librbd.so` in shared library include paths. This works well with devel Ceph packages that install `librbd.so` symlinks, but typically library-only packages instead have it with a version postfix. You can disable this automatic linking by passing `--cfg ceph_sys_nolink` to rustc and link to the exact desired version yourself.

## Feature Flags

To ensure that consumers can actually expect specific APIs to be available, the crate provides the following feature flags:

### Minimum API version features (starting from 1.12.0)
By specifying one of these features, the crate will fail to compile if the librbd C headers define an API-incompatible version:

| feature | librbd   | ceph      |
|---------|----------|-----------|
| `v1-12` | = 1.12.x | = 12.≥1.x |
| `v1-15` | = 1.15.x | = 15.≥1.x |
| `v1-17` | ≥ 1.17.0 | ≥ 17.1.0  |
| `v1-18` | ≥ 1.18.0 | ≥ 18.1.0  |
| `v1-19` | ≥ 1.19.0 | ≥ 19.1.0  |
| `v1-20` | ≥ 1.20.0 | ≥ 20.1.0  |

Note that although there appears to be a pattern between Ceph and librbd versions above, this is coincidental. The Ceph versions are provided for information only and should not be solely relied upon for determining the librbd version.

### Fine-grained API support defines

These features require that the associated `LIBRBD_SUPPORT_` define is present in the librbd C headers:

| feature | librbd |
|---------|--------|
| `aio_flush` | \* |
| `aio_open` | \* |
| `locking` | \* |
| `watch` | \* |
| `invalidate` | ≥ 1.12.0 |
| `iovec` | ≥ 1.12.0 |
| `compare_and_write` | ≥ 1.15.0 |
| `writesame` | ≥ 1.15.0 |
| `write_zeroes` | ≥ 1.17.0 |
| `encryption` | ≥ 1.18.0 |
| `encryption_load2` | ≥ 1.19.0 |
| `compare_and_write_iovec` | ≥ 1.19.0 |
| `group_snap_get_info` | ≥ 1.20.0[^1] |
| `diff_iterate3` | ≥ 1.20.0[^2] |

[^1]: Supported by headers shipping with Ceph 20.0.0.
[^2]: Backported to librbd headers of Ceph 19.2.3 and 18.2.7.

# Licensing

The `bindings.rs` file in this repository consists of machine-translated header files from the [Ceph](https://github.com/ceph/ceph) project, version 20.2.4, specifically `src/include/rbd/librbd.h` and `src/include/rbd/features.h`. These are licensed under the GNU Lesser General Public License, version 3.0 (LGPL-3.0), and can be obtained at <https://github.com/ceph/ceph/tree/v20.2.4/src/include/rbd>.
