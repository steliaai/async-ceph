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
    use std::{
        cell::RefCell,
        collections::HashSet,
        env,
        fs,
        hash::{BuildHasherDefault, DefaultHasher},
        io::Write,
        path::PathBuf,
        rc::Rc,
    };

    pub fn generate_bindings() -> Option<()> {
        let api_info: Rc<RefCell<_>> = Rc::default();

        let bindings = ceph_bindings_gen::BindingsConfig {
            system_library_name: "rbd",
            system_header_file: "include/librbd.h",
            header_files: &["rbd/librbd.h", "rbd/features.h"],
        }
        .configure_bindgen()?
        .parse_callbacks(Box::new(ApiInfoCollector(api_info.clone())))
        .allowlist_function("(rbd)_.*")
        .allowlist_var("(RBD|CEPH|LIBRBD)_.*")
        .generate()
        .expect("Unable to generate bindings");

        let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&out_path)
            .expect("Couldn't open bindings.rs for writing");

        // before generating bindings, check for requested API level
        let api_info = api_info.borrow();
        if !RequestedApiInfo::from_env().ensure_compat(&api_info) {
            return None;
        }

        bindings.write(Box::new(&mut file)).expect("Couldn't write bindings");
        api_info.generate_const_helpers(file);

        #[cfg(ceph_sys_pregen_bindings)]
        {
            let dest_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/bindings.rs");
            std::fs::copy(out_path, dest_path).unwrap();
        }

        Some(())
    }

    /// Small helper to print version triples.
    fn version_triple(version: (u32, u32, u32)) -> String {
        format!("{}.{}.{}", version.0, version.1, version.2)
    }

    /// Information about the librbd API surface.
    #[derive(Debug, Default)]
    struct ApiInfo {
        version_triple: (u32, u32, u32),
        features: HashSet<String, BuildHasherDefault<DefaultHasher>>,
    }

    impl ApiInfo {
        /// Generate constants and macros that can be used by the consumer to check feature support
        /// at compile time.
        pub fn generate_const_helpers(&self, mut writer: impl Write) {
            // write support flags
            let features_as_vec: Vec<_> = self.features.iter().collect();
            writeln!(writer, "pub(crate) const _LIBRBD_SUPPORTS: &[&str] = &{features_as_vec:?};")
                .expect("Couldn't write support feature set");

            // while it's possible, checking if a slice contains a string in a const context is
            // very verbose due to the lack of iterator or trait support.
            //
            // generate a helper function that does a byte-level match instead.
            let match_expr = self.features.iter().map(|f| format!("b{f:?}")).collect::<Vec<_>>().join("|");
            writeln!(
                writer,
                "pub(crate) const fn _supports(feature: &str) -> bool {{ matches!(feature.as_bytes(), {match_expr}) }}"
            )
            .expect("Couldn't write support feature check function");

            // generate a decl macro that outputs a librbd version literal
            // here we write the doc inline on a public item because the macro has to be public
            let version_str = version_triple(self.version_triple);
            writeln!(
                writer,
                r#"
                /// Macro that returns the librbd version as a semver string literal e.g. `"1.2.3"`.
                ///
                /// This can be used to improve error messages in custom version checking by including
                /// the librbd version in a `compile_error!` or const `panic!` message.
                #[macro_export]
                macro_rules! version_lit {{
                    () => {{ {version_str:?} }}
                }}"#
            )
            .expect("Couldn't write version literal macro");
        }
    }

    /// collects LIBRBD_SUPPORTS defines and version macros into an `ApiInfo` struct.
    #[derive(Debug)]
    struct ApiInfoCollector(Rc<RefCell<ApiInfo>>);

    impl bindgen::callbacks::ParseCallbacks for ApiInfoCollector {
        fn int_macro(&self, name: &str, value: i64) -> Option<bindgen::callbacks::IntKind> {
            let mut api_info = self.0.borrow_mut();
            if let Some(feature) = name.strip_prefix("LIBRBD_SUPPORTS_") {
                api_info.features.insert(feature.into());
            }
            match name {
                "LIBRBD_VER_MAJOR" => api_info.version_triple.0 = value as u32,
                "LIBRBD_VER_MINOR" => api_info.version_triple.1 = value as u32,
                "LIBRBD_VER_EXTRA" => api_info.version_triple.2 = value as u32,
                _ => (),
            }
            None
        }
    }

    /// A minimum rbd version requested by a Rust feature.
    #[derive(Debug)]
    struct VersionFeature {
        string: String,
        version_triple: (u32, u32, u32),
    }

    /// Information about the requested minimum API surface.
    struct RequestedApiInfo {
        support_features: Vec<String>,
        version_features: Vec<VersionFeature>,
    }

    impl RequestedApiInfo {
        /// Collect the minimum API surface from the enabled crate features that cargo
        /// exposes to the build script via environment variables.
        pub fn from_env() -> Self {
            let mut support_features = std::env::vars()
                .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(|f| f.to_owned()))
                .collect::<Vec<_>>();

            let version_re = regex_lite::Regex::new(r"^V(\d+)_(\d+)(?:_(\d+))?$").unwrap();
            let mut version_features = vec![];
            support_features.retain(|feat| {
                if let Some(caps) = version_re.captures(feat) {
                    version_features.push(VersionFeature {
                        string: feat.clone().to_lowercase().replace('_', "-"),
                        version_triple: (
                            caps[1].parse().unwrap(),
                            caps[2].parse().unwrap(),
                            caps.get(3).map_or(0, |m| m.as_str().parse().unwrap()),
                        ),
                    });
                    return false;
                }
                true
            });

            Self {
                support_features,
                version_features,
            }
        }

        /// Verify that the requested APIs are provided by the system header.
        pub fn ensure_compat(&self, actual: &ApiInfo) -> bool {
            let mut compat = true;

            // version checks
            if !self.version_features.is_empty() {
                // checks that code for version 'low' is API-compatible with version 'high'
                // librbd seems to follow semver re: ABI compat, but not API, so we
                // have to do a little hardcoding here
                fn is_api_compat(low: (u32, u32, u32), high: (u32, u32, u32)) -> bool {
                    if low.0 != high.0 {
                        false
                    } else if low.0 == 0 {
                        low == high
                    } else {
                        // the api has been forward compatible since 1.17.x
                        low <= high && (low.1 == high.1 || (low.0 == 1 && low.1 >= 17))
                    }
                }

                let min_ver = self.version_features.iter().min_by_key(|v| v.version_triple).unwrap();
                let max_ver = self.version_features.iter().max_by_key(|v| v.version_triple).unwrap();

                // First, ensure the user didn't enable API-incompatible version features
                // to check this it suffices to check the min and max versions
                if !is_api_compat(min_ver.version_triple, max_ver.version_triple) {
                    println!(
                        "cargo::error=two API-incompatible version features enabled: '{}' and '{}'",
                        min_ver.string, max_ver.string
                    );
                    compat = false;
                }

                // Then, check that the max requested version is compatible with librbd's
                if !is_api_compat(max_ver.version_triple, actual.version_triple) {
                    println!(
                        "cargo::error=minimum librbd version feature '{}' ({}) is not API-compatible with the \
                        librbd headers (version {})",
                        max_ver.string,
                        version_triple(max_ver.version_triple),
                        version_triple(actual.version_triple)
                    );
                    compat = false;
                }
            }

            // support flag checks
            for s in &self.support_features {
                if !actual.features.contains(s) {
                    println!(
                        "cargo::error=requested librbd feature '{}' is not supported by the librbd headers",
                        s.to_lowercase()
                    )
                }
            }

            compat
        }
    }
}
