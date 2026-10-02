// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() {
    #[cfg(not(ceph_sys_bundled))]
    generate_bindings();
}

#[cfg(not(ceph_sys_bundled))]
pub fn generate_bindings() -> Option<()> {
    use std::{env, path::PathBuf};

    let bindings = ceph_bindings_gen::BindingsConfig {
        system_library_name: "rados",
        system_header_file: "include/librados.h",
        header_files: &["rados/librados.h"],
    }
    .configure_bindgen()?
    .allowlist_function("rados_.*")
    .allowlist_var("(RADOS|CEPH|LIBRADOS)_.*")
    .generate()
    .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings.write_to_file(&out_path).expect("Couldn't write bindings!");

    #[cfg(ceph_sys_pregen_bindings)]
    {
        let dest_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/bindings.rs");
        std::fs::copy(out_path, dest_path).unwrap();
    }

    Some(())
}
