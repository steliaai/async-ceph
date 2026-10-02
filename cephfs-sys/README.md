# cephfs-sys

Low-level bindings to Ceph's `libcephfs`.

# Building

`cephfs-sys` is designed to support a wide set of libcephfs versions, and as such relies on `bindgen` to generate its bindings from the libcephfs headers available on the host system. By default it pulls these headers from system include paths, and links to `libcephfs.so` in system shared library paths.

This means that for the crate to build with its default configuration, on Debian you will need the following packages to be installed on your system:

- `libcephfs-dev`
- `libclang-**-dev` (for `bindgen`)

On alpine, you will need

- `ceph**-dev`
- `linux-headers` (used by `libcephfs`)
- `clang**-libclang` (for `bindgen`)

However, it's possible to get away with less by passing custom `cfg` directives to rustc as explained below.

# Configuration

## Headers

By default, `cephfs-sys` will attempt to find the libcephfs C headers in the system include paths. This behavior can be overridden in two mutually incompatible ways:
- By passing `--cfg ceph_sys_bundled` to rustc. This will use pregenerated bindings fetched from the latest stable Ceph release at the time the crate was released (currently 20.2.4). This option removes the need for `libclang` and `linux-headers` packages.
- By passing `--cfg ceph_sys_include=/asbolute/path/to/ceph/src/include` to rustc, where `include/cephfs` is a folder that contains the libcephfs include files. **This path must be absolute!**

Using any of these cfgs will also disable linking to `libcephfs.so` in system paths, as documented below.

## Linking

By default, `cephfs-sys` will emit `cargo:rustc-link-lib=cephfs` to dynamically link to `libcephfs.so` in shared library include paths. This works well with devel Ceph packages that install `libcephfs.so` symlinks, but typically library-only packages instead have it with a version postfix. You can disable this automatic linking by passing `--cfg ceph_sys_nolink` to rustc and link to the exact desired version yourself.

# Licensing

The `bindings.rs` file in this repository consists of machine-translated header files from the [Ceph](https://github.com/ceph/ceph) project, version 20.2.4, specifically `src/include/cephfs/libcephfs.h`. These are licensed under the GNU Lesser General Public License, version 3.0 (LGPL-3.0), and can be obtained at <https://github.com/ceph/ceph/tree/v20.2.4/src/include/cephfs>.
