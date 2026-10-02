// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() {
    #[cfg(not(ceph_sys_bundled))]
    generator::generate_bindings();
}

#[cfg(not(ceph_sys_bundled))]
mod generator {
    use std::{env, fs, path::PathBuf};

    // Additional defines to be added to the header.
    // The xattr flags in question are not part of libcephfs.h for some reason.
    // It's not clear why `_FILE_OFFSET_BITS` is needed, but the Rust code would
    // fail to compile if the bit width is unexpected, so just add it here.
    static EXTRA_DEFINES: &str = "
    #define _FILE_OFFSET_BITS 64
    #define CEPH_XATTR_CREATE  (1 << 0)
    #define CEPH_XATTR_REPLACE (1 << 1)
    ";

    pub fn generate_bindings() -> Option<()> {
        let bindings = ceph_bindings_gen::BindingsConfig {
            system_library_name: "cephfs",
            system_header_file: "include/libcephfs.h",
            header_files: &["cephfs/libcephfs.h"],
        }
        .configure_bindgen()?
        .allowlist_function("(ceph)_.*")
        .allowlist_var("(CEPHFS|CEPH)_.*")
        .header_contents("extra_defines.h", EXTRA_DEFINES)
        .generate()
        .expect("Unable to generate bindings");

        let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&out_path)
            .expect("Couldn't open bindings.rs for writing");

        bindings.write(Box::new(&mut file)).expect("Couldn't write bindings");

        #[cfg(ceph_sys_pregen_bindings)]
        {
            let dest_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/bindings.rs");
            std::fs::copy(out_path, dest_path).unwrap();
        }

        Some(())
    }
}
