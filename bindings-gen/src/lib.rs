// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bindgen config generator for the async-ceph sys crates.
//!
//! This is not meant to be used by other crates.

use rustflags::Flag;
use std::path::PathBuf;

pub mod doxygen_format;

/// Build options that can be controlled by the crate user.
#[derive(Debug, Clone)]
struct UserFlags {
    pub include_dir: Option<PathBuf>,
    pub link: bool,
}

impl UserFlags {
    pub fn from_rustflags() -> Option<Self> {
        let mut this = Self {
            include_dir: None,
            link: true,
        };

        for flag in rustflags::from_env() {
            match flag {
                Flag::Cfg { name, .. } if name == "ceph_sys_nolink" => this.link = false,
                Flag::Cfg { name, .. } if name == "ceph_sys_bundled" => return None,
                // pulling headers from a custom include path
                Flag::Cfg { name, value } if name == "ceph_sys_include" => {
                    if this.include_dir.is_some() {
                        println!("cargo::error=ceph_sys_include may only be specified once");
                        return None;
                    }
                    let Some(path) = value else {
                        println!(
                            "cargo::error=the path to the Ceph include directories must be set for the \
                            ceph_sys_include cfg",
                        );
                        return None;
                    };
                    // make sure the path is absolute, since we have no way to know the user's
                    // work directory at the time of running the cargo command
                    let include_dir = PathBuf::from(path);
                    if !include_dir.is_absolute() {
                        println!("cargo::error=ceph_sys_include: expected an absolute path, got '{}'", include_dir.display());
                        return None;
                    };
                    if !include_dir.is_dir() {
                        println!("cargo::error=ceph_sys_include: '{}' is not a directory", include_dir.display());
                        return None;
                    };

                    this.include_dir = Some(include_dir);
                }
                _ => (),
            }
        }
        Some(this)
    }
}

/// Configuration for a -sys crate that async-ceph depends on.
pub struct BindingsConfig<'a> {
    /// Name of the system library to link to by default.
    pub system_library_name: &'a str,
    /// Path to the default header file that pulls from system include directories.
    pub system_header_file: &'a str,
    /// Relative paths to the headers that need to be included from the path provided by the user.
    pub header_files: &'a [&'a str],
}

impl BindingsConfig<'_> {
    const SPDX_LICENSE_ID: &'static str = "// SPDX-License-Identifier: LGPL-3.0-only";

    /// Configure a bindgen builder based on this build configuration.
    ///
    /// Returns [`None`] if basic sanity checks fail or code generation should not run.
    pub fn configure_bindgen(self) -> Option<bindgen::Builder> {
        let user_flags = UserFlags::from_rustflags()?;

        // A docsrs build from a dependent crate might not set the `ceph_sys_bundled` cfg,
        // which will break its docs.rs codegen. Thus we should default to bundled bindings instead
        // of system headers when building the crate on docs-rs.
        //
        // The canonical way to detect docs-rs from a build script is with the DOCS_RS env var
        // (see https://github.com/rust-lang/docs.rs/issues/147)
        if user_flags.include_dir.is_none() && std::env::var("DOCS_RS").is_ok() {
            println!("cargo:rustc-cfg=ceph_sys_bundled");
            return None;
        }

        let headers: Vec<PathBuf> = user_flags
            .include_dir
            .map_or_else(|| vec![self.system_header_file.into()], |p| self.header_files.iter().map(|h| p.join(h)).collect());

        let missing: Vec<_> = headers
            .iter()
            .filter(|h| std::fs::metadata(h).ok().is_none_or(|f| !f.is_file()))
            .map(|h| h.to_string_lossy())
            .collect();

        if !missing.is_empty() {
            println!("cargo::error=librbd header(s) '{}' not a file or could not be found", missing.join(", "));
            return None;
        }

        if user_flags.link {
            println!("cargo:rustc-link-lib={}", self.system_library_name);
        }

        Some(
            bindgen::Builder::default()
                .raw_line(Self::SPDX_LICENSE_ID)
                .headers(headers.iter().map(|h| h.to_str().unwrap()))
                .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
                .parse_callbacks(Box::new(doxygen_format::DoxygenFormatter))
                // New members can be added over time, so we can't generate Rust enums as it
                // would be unsound to create the enum type with that unknown variant, even with repr(u*)
                .default_enum_style(bindgen::EnumVariation::NewType {
                    is_bitfield: false,
                    is_global: false,
                })
                .constified_enum("_bindgen_ty_.*")
                .clang_arg("-fretain-comments-from-system-headers"),
        )
    }
}
