// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use autocxx_parser::file_locations::FileLocationStrategy;
use proc_macro2::TokenStream;

use crate::{ParseError, RebuildDependencyRecorder};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{ffi::OsStr, io, process};
use std::{fmt::Display, fs::File};

/// Errors returned during creation of a cc::Build from an include_cxx
/// macro.
#[derive(Debug)]
pub enum BuilderError {
    /// The cxx module couldn't parse the code generated by autocxx.
    /// This could well be a bug in autocxx.
    InvalidCxx(cxx_gen::Error),
    /// The .rs file didn't exist or couldn't be parsed.
    ParseError(ParseError),
    /// We couldn't write the c++ code to disk.
    FileWriteFail(std::io::Error, PathBuf),
    /// No `include_cxx` macro was found anywhere.
    NoIncludeCxxMacrosFound,
    /// Unable to create one of the directories to which we need to write
    UnableToCreateDirectory(std::io::Error, PathBuf),
}

impl Display for BuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuilderError::ParseError(pe) => write!(f, "Unable to parse .rs file: {}", pe)?,
            BuilderError::InvalidCxx(ee) => write!(f, "cxx was unable to understand the code generated by autocxx (likely a bug in autocxx; please report.) {}", ee)?,
            BuilderError::FileWriteFail(ee, pb) => write!(f, "Unable to write to {}: {}", pb.to_string_lossy(), ee)?,
            BuilderError::NoIncludeCxxMacrosFound => write!(f, "No include_cpp! macro found")?,
            BuilderError::UnableToCreateDirectory(ee, pb) => write!(f, "Unable to create directory {}: {}", pb.to_string_lossy(), ee)?,
        }
        Ok(())
    }
}

pub type BuilderBuild = cc::Build;

pub struct BuilderSuccess(pub BuilderBuild, pub Vec<PathBuf>);

/// Results of a build.
pub type BuilderResult = Result<BuilderSuccess, BuilderError>;

/// An object to allow building of bindings from a `build.rs` file.
pub struct Builder {
    rs_file: PathBuf,
    autocxx_incs: Vec<OsString>,
    extra_clang_args: Vec<String>,
    dependency_recorder: Option<Box<dyn RebuildDependencyRecorder>>,
    custom_gendir: Option<PathBuf>,
    auto_allowlist: bool,
}

impl Builder {
    #[doc(hidden)]
    pub fn new_internal(
        rs_file: impl AsRef<Path>,
        autocxx_incs: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> Self {
        Self {
            rs_file: rs_file.as_ref().to_path_buf(),
            autocxx_incs: autocxx_incs
                .into_iter()
                .map(|s| s.as_ref().to_os_string())
                .collect(),
            extra_clang_args: Vec::new(),
            dependency_recorder: None,
            custom_gendir: None,
            auto_allowlist: false,
        }
    }

    /// Specify extra arguments for clang.
    pub fn extra_clang_args(mut self, extra_clang_args: &[&str]) -> Self {
        self.extra_clang_args = extra_clang_args.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Where to generate the code.
    pub fn custom_gendir(mut self, custom_gendir: PathBuf) -> Self {
        self.custom_gendir = Some(custom_gendir);
        self
    }

    /// Specify an object to be informed of which header files this build depends upon.
    pub fn dependency_recorder(
        mut self,
        dependency_recorder: Box<dyn RebuildDependencyRecorder>,
    ) -> Self {
        self.dependency_recorder = Some(dependency_recorder);
        self
    }

    /// Automatically discover uses of the C++ `ffi` mod and generate the allowlist
    /// from that.
    pub fn auto_allowlist(mut self, do_it: bool) -> Self {
        self.auto_allowlist = do_it;
        self
    }

    /// Build autocxx C++ files and return a cc::Build you can use to build
    /// more from a build.rs file.
    pub fn build(self) -> Result<BuilderBuild, BuilderError> {
        self.build_listing_files().map(|r| r.0)
    }

    pub(crate) fn build_listing_files(self) -> Result<BuilderSuccess, BuilderError> {
        let clang_args = &self
            .extra_clang_args
            .iter()
            .map(|s| &s[..])
            .collect::<Vec<_>>();
        rust_version_check();
        let gen_location_strategy = match self.custom_gendir {
            None => FileLocationStrategy::new(),
            Some(custom_dir) => FileLocationStrategy::Custom(custom_dir),
        };
        let incdir = gen_location_strategy.get_include_dir();
        ensure_created(&incdir)?;
        let cxxdir = gen_location_strategy.get_cxx_dir();
        ensure_created(&cxxdir)?;
        let rsdir = gen_location_strategy.get_rs_dir();
        ensure_created(&rsdir)?;
        // We are incredibly unsophisticated in our directory arrangement here
        // compared to cxx. I have no doubt that we will need to replicate just
        // about everything cxx does, in due course...
        // Write cxx.h to that location, as it may be needed by
        // some of our generated code.
        write_to_file(&incdir, "cxx.h", crate::HEADER.as_bytes())?;

        let autocxx_inc = build_autocxx_inc(self.autocxx_incs, &incdir);
        // pass on th
        gen_location_strategy.set_cargo_env_vars_for_build();

        let mut parsed_file = crate::parse_file(self.rs_file, self.auto_allowlist)
            .map_err(BuilderError::ParseError)?;
        parsed_file
            .resolve_all(autocxx_inc, clang_args, self.dependency_recorder)
            .map_err(BuilderError::ParseError)?;
        let mut counter = 0;
        let mut builder = cc::Build::new();
        builder.cpp(true);
        let mut generated_rs = Vec::new();
        builder.includes(parsed_file.include_dirs());
        for include_cpp in parsed_file.get_cpp_buildables() {
            let generated_code = include_cpp
                .generate_h_and_cxx()
                .map_err(BuilderError::InvalidCxx)?;
            for filepair in generated_code.0 {
                let fname = format!("gen{}.cxx", counter);
                counter += 1;
                let gen_cxx_path = write_to_file(&cxxdir, &fname, &filepair.implementation)?;
                builder.file(gen_cxx_path);

                write_to_file(&incdir, &filepair.header_name, &filepair.header)?;
            }
        }

        for include_cpp in parsed_file.get_rs_buildables() {
            let rs = include_cpp.generate_rs();
            generated_rs.push(write_rs_to_file(
                &rsdir,
                &include_cpp.config.get_rs_filename(),
                rs,
            )?);
        }
        if counter == 0 {
            Err(BuilderError::NoIncludeCxxMacrosFound)
        } else {
            Ok(BuilderSuccess(builder, generated_rs))
        }
    }

    /// Builds successfully, or exits the process displaying a suitable
    /// message.
    pub fn expect_build(self) -> BuilderBuild {
        self.build().unwrap_or_else(|err| {
            let _ = writeln!(io::stderr(), "\n\nautocxx error: {}\n\n", err);
            process::exit(1);
        })
    }
}

fn ensure_created(dir: &Path) -> Result<(), BuilderError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| BuilderError::UnableToCreateDirectory(e, dir.to_path_buf()))
}

fn build_autocxx_inc<I, T>(paths: I, extra_path: &Path) -> Vec<PathBuf>
where
    I: IntoIterator<Item = T>,
    T: AsRef<OsStr>,
{
    paths
        .into_iter()
        .map(|p| PathBuf::from(p.as_ref()))
        .chain(std::iter::once(extra_path.to_path_buf()))
        .collect()
}

fn write_to_file(dir: &Path, filename: &str, content: &[u8]) -> Result<PathBuf, BuilderError> {
    let path = dir.join(filename);
    try_write_to_file(&path, content).map_err(|e| BuilderError::FileWriteFail(e, path.clone()))?;
    Ok(path)
}

fn try_write_to_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(content)
}

fn write_rs_to_file(
    dir: &Path,
    filename: &str,
    content: TokenStream,
) -> Result<PathBuf, BuilderError> {
    write_to_file(dir, filename, content.to_string().as_bytes())
}

fn rust_version_check() {
    if !version_check::is_min_version("1.48.0").unwrap_or(false) {
        panic!("Rust 1.48 or later is required.")
    }
}
