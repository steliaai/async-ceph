# rados-sys

Low-level bindings to Ceph's `librados`.

# Building

`rados-sys` is designed to support a wide set of librados versions, and as such relies on `bindgen` to generate its bindings from the librados headers available on the host system. By default it pulls these headers from system include paths, and links to `librados.so` in system shared library paths.

This means that for the crate to build with its default configuration, on Debian you will need the following packages to be installed on your system:

- `librados-dev`
- `libclang-**-dev` (for `bindgen`)

On alpine, you will need

- `ceph**-dev`
- `linux-headers` (used by `librados`)
- `clang**-libclang` (for `bindgen`)

However, it's possible to get away with less by passing custom `cfg` directives to rustc as explained below.

# Configuration

## Headers

By default, `rados-sys` will attempt to find the librados C headers in the system include paths. This behavior can be overridden in two mutually incompatible ways:
- By passing `--cfg ceph_sys_bundled` to rustc. This will use pregenerated bindings fetched from the latest stable Ceph release at the time the crate was released (currently 20.2.4). This option removes the need for `libclang` and `linux-headers` packages.
- By passing `--cfg ceph_sys_include=/asbolute/path/to/ceph/src/include` to rustc, where `include/rados` is a folder that contains the librados include files. **This path must be absolute!**

Using any of these cfgs will also disable linking to `librados.so` in system paths, as documented below.

## Linking

By default, `rados-sys` will emit `cargo:rustc-link-lib=rados` to dynamically link to `librados.so` in shared library include paths. This works well with devel Ceph packages that install `librados.so` symlinks, but typically library-only packages instead have it with a version postfix. You can disable this automatic linking by passing `--cfg ceph_sys_nolink` to rustc and link to the exact desired version yourself.

# Licensing

The `bindings.rs` file in this repository consists of machine-translated header files from the [Ceph](https://github.com/ceph/ceph) project, version 20.2.4, specifically `src/include/rados/librados.h` and `src/include/rados/rados_types.h`. These are licensed under the GNU Lesser General Public License, version 3.0 (LGPL-3.0), and can be obtained at <https://github.com/ceph/ceph/tree/v20.2.4/src/include/rados>.
