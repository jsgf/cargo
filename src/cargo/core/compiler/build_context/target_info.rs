use crate::core::compiler::{
    BuildOutput, CompileKind, CompileMode, CompileTarget, Context, CrateType,
};
use crate::core::{Dependency, Target, TargetKind, Workspace};
use crate::util::config::{Config, StringList, TargetConfig};
use crate::util::{CargoResult, Rustc};
use anyhow::Context as _;
use cargo_platform::{Cfg, CfgExpr};
use cargo_util::{paths, ProcessBuilder};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::hash_map::{Entry, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::str::{self, FromStr};

/// Information about the platform target gleaned from querying rustc.
///
/// `RustcTargetData` keeps two of these, one for the host and one for the
/// target. If no target is specified, it uses a clone from the host.
#[derive(Clone)]
pub struct TargetInfo {
    /// A base process builder for discovering crate type information. In
    /// particular, this is used to determine the output filename prefix and
    /// suffix for a crate type.
    crate_type_process: ProcessBuilder,
    /// Cache of output filename prefixes and suffixes.
    ///
    /// The key is the crate type name (like `cdylib`) and the value is
    /// `Some((prefix, suffix))`, for example `libcargo.so` would be
    /// `Some(("lib", ".so")). The value is `None` if the crate type is not
    /// supported.
    crate_types: RefCell<HashMap<CrateType, Option<(String, String)>>>,
    /// `cfg` information extracted from `rustc --print=cfg`.
    cfg: Vec<Cfg>,
    /// Path to the sysroot.
    pub sysroot: PathBuf,
    /// Path to the "lib" or "bin" directory that rustc uses for its dynamic
    /// libraries.
    pub sysroot_host_libdir: PathBuf,
    /// Path to the "lib" directory in the sysroot which rustc uses for linking
    /// target libraries.
    pub sysroot_target_libdir: PathBuf,
    /// Extra flags to pass to `rustc`, see `env_args`.
    pub rustflags: Vec<String>,
    /// Extra flags to pass to `rustdoc`, see `env_args`.
    pub rustdocflags: Vec<String>,
    /// Whether or not rustc supports the `-Csplit-debuginfo` flag.
    pub supports_split_debuginfo: bool,
}

/// Kind of each file generated by a Unit, part of `FileType`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileFlavor {
    /// Not a special file type.
    Normal,
    /// Like `Normal`, but not directly executable.
    /// For example, a `.wasm` file paired with the "normal" `.js` file.
    Auxiliary,
    /// Something you can link against (e.g., a library).
    Linkable,
    /// An `.rmeta` Rust metadata file.
    Rmeta,
    /// An `.rcheck` Rust metadata file. Like rmeta, but only ever useful for check.
    Rcheck,
    /// Piece of external debug information (e.g., `.dSYM`/`.pdb` file).
    DebugInfo,
}

/// Type of each file generated by a Unit.
#[derive(Debug)]
pub struct FileType {
    /// The kind of file.
    pub flavor: FileFlavor,
    /// The crate-type that generates this file.
    ///
    /// `None` for things that aren't associated with a specific crate type,
    /// for example `rmeta` files.
    pub crate_type: Option<CrateType>,
    /// The suffix for the file (for example, `.rlib`).
    /// This is an empty string for executables on Unix-like platforms.
    suffix: String,
    /// The prefix for the file (for example, `lib`).
    /// This is an empty string for things like executables.
    prefix: String,
    /// Flag to convert hyphen to underscore when uplifting.
    should_replace_hyphens: bool,
}

impl FileType {
    /// The filename for this FileType crated by rustc.
    pub fn output_filename(&self, target: &Target, metadata: Option<&str>) -> String {
        match metadata {
            Some(metadata) => format!(
                "{}{}-{}{}",
                self.prefix,
                target.crate_name(),
                metadata,
                self.suffix
            ),
            None => format!("{}{}{}", self.prefix, target.crate_name(), self.suffix),
        }
    }

    /// The filename for this FileType that Cargo should use when "uplifting"
    /// it to the destination directory.
    pub fn uplift_filename(&self, target: &Target) -> String {
        let name = if self.should_replace_hyphens {
            target.crate_name()
        } else {
            target.name().to_string()
        };
        format!("{}{}{}", self.prefix, name, self.suffix)
    }

    /// Creates a new instance representing a `.rmeta` file.
    pub fn new_rmeta() -> FileType {
        // Note that even binaries use the `lib` prefix.
        FileType {
            flavor: FileFlavor::Rmeta,
            crate_type: None,
            suffix: ".rmeta".to_string(),
            prefix: "lib".to_string(),
            should_replace_hyphens: true,
        }
    }

    /// Creates a new instance representing a `.rcheck` file.
    pub fn new_rcheck() -> FileType {
        // Note that even binaries use the `lib` prefix.
        FileType {
            flavor: FileFlavor::Rcheck,
            crate_type: None,
            suffix: ".rcheck".to_string(),
            prefix: "lib".to_string(),
            should_replace_hyphens: true,
        }
    }
}

impl TargetInfo {
    pub fn new(
        config: &Config,
        requested_kinds: &[CompileKind],
        rustc: &Rustc,
        kind: CompileKind,
    ) -> CargoResult<TargetInfo> {
        let rustflags = env_args(
            config,
            requested_kinds,
            &rustc.host,
            None,
            kind,
            "RUSTFLAGS",
        )?;
        let extra_fingerprint = kind.fingerprint_hash();
        let mut process = rustc.workspace_process();
        process
            .arg("-")
            .arg("--crate-name")
            .arg("___")
            .arg("--print=file-names")
            .args(&rustflags)
            .env_remove("RUSTC_LOG");

        if let CompileKind::Target(target) = kind {
            process.arg("--target").arg(target.rustc_target());
        }

        let crate_type_process = process.clone();
        const KNOWN_CRATE_TYPES: &[CrateType] = &[
            CrateType::Bin,
            CrateType::Rlib,
            CrateType::Dylib,
            CrateType::Cdylib,
            CrateType::Staticlib,
            CrateType::ProcMacro,
        ];
        for crate_type in KNOWN_CRATE_TYPES.iter() {
            process.arg("--crate-type").arg(crate_type.as_str());
        }
        let supports_split_debuginfo = rustc
            .cached_output(
                process.clone().arg("-Csplit-debuginfo=packed"),
                extra_fingerprint,
            )
            .is_ok();

        process.arg("--print=sysroot");
        process.arg("--print=cfg");

        let (output, error) = rustc
            .cached_output(&process, extra_fingerprint)
            .with_context(|| "failed to run `rustc` to learn about target-specific information")?;

        let mut lines = output.lines();
        let mut map = HashMap::new();
        for crate_type in KNOWN_CRATE_TYPES {
            let out = parse_crate_type(crate_type, &process, &output, &error, &mut lines)?;
            map.insert(crate_type.clone(), out);
        }

        let line = match lines.next() {
            Some(line) => line,
            None => anyhow::bail!(
                "output of --print=sysroot missing when learning about \
                 target-specific information from rustc\n{}",
                output_err_info(&process, &output, &error)
            ),
        };
        let sysroot = PathBuf::from(line);
        let sysroot_host_libdir = if cfg!(windows) {
            sysroot.join("bin")
        } else {
            sysroot.join("lib")
        };
        let mut sysroot_target_libdir = sysroot.clone();
        sysroot_target_libdir.push("lib");
        sysroot_target_libdir.push("rustlib");
        sysroot_target_libdir.push(match &kind {
            CompileKind::Host => rustc.host.as_str(),
            CompileKind::Target(target) => target.short_name(),
        });
        sysroot_target_libdir.push("lib");

        let cfg = lines
            .map(|line| Ok(Cfg::from_str(line)?))
            .filter(TargetInfo::not_user_specific_cfg)
            .collect::<CargoResult<Vec<_>>>()
            .with_context(|| {
                format!(
                    "failed to parse the cfg from `rustc --print=cfg`, got:\n{}",
                    output
                )
            })?;

        Ok(TargetInfo {
            crate_type_process,
            crate_types: RefCell::new(map),
            sysroot,
            sysroot_host_libdir,
            sysroot_target_libdir,
            // recalculate `rustflags` from above now that we have `cfg`
            // information
            rustflags: env_args(
                config,
                requested_kinds,
                &rustc.host,
                Some(&cfg),
                kind,
                "RUSTFLAGS",
            )?,
            rustdocflags: env_args(
                config,
                requested_kinds,
                &rustc.host,
                Some(&cfg),
                kind,
                "RUSTDOCFLAGS",
            )?,
            cfg,
            supports_split_debuginfo,
        })
    }

    fn not_user_specific_cfg(cfg: &CargoResult<Cfg>) -> bool {
        if let Ok(Cfg::Name(cfg_name)) = cfg {
            // This should also include "debug_assertions", but it causes
            // regressions. Maybe some day in the distant future it can be
            // added (and possibly change the warning to an error).
            if cfg_name == "proc_macro" {
                return false;
            }
        }
        true
    }

    /// All the target `cfg` settings.
    pub fn cfg(&self) -> &[Cfg] {
        &self.cfg
    }

    /// Returns the list of file types generated by the given crate type.
    ///
    /// Returns `None` if the target does not support the given crate type.
    fn file_types(
        &self,
        crate_type: &CrateType,
        flavor: FileFlavor,
        target_triple: &str,
    ) -> CargoResult<Option<Vec<FileType>>> {
        let crate_type = if *crate_type == CrateType::Lib {
            CrateType::Rlib
        } else {
            crate_type.clone()
        };

        let mut crate_types = self.crate_types.borrow_mut();
        let entry = crate_types.entry(crate_type.clone());
        let crate_type_info = match entry {
            Entry::Occupied(o) => &*o.into_mut(),
            Entry::Vacant(v) => {
                let value = self.discover_crate_type(v.key())?;
                &*v.insert(value)
            }
        };
        let (prefix, suffix) = match *crate_type_info {
            Some((ref prefix, ref suffix)) => (prefix, suffix),
            None => return Ok(None),
        };
        let mut ret = vec![FileType {
            suffix: suffix.clone(),
            prefix: prefix.clone(),
            flavor,
            crate_type: Some(crate_type.clone()),
            should_replace_hyphens: crate_type != CrateType::Bin,
        }];

        // Window shared library import/export files.
        if crate_type.is_dynamic() {
            // Note: Custom JSON specs can alter the suffix. For now, we'll
            // just ignore non-DLL suffixes.
            if target_triple.ends_with("-windows-msvc") && suffix == ".dll" {
                // See https://docs.microsoft.com/en-us/cpp/build/reference/working-with-import-libraries-and-export-files
                // for more information about DLL import/export files.
                ret.push(FileType {
                    suffix: ".dll.lib".to_string(),
                    prefix: prefix.clone(),
                    flavor: FileFlavor::Auxiliary,
                    crate_type: Some(crate_type.clone()),
                    should_replace_hyphens: true,
                });
                // NOTE: lld does not produce these
                ret.push(FileType {
                    suffix: ".dll.exp".to_string(),
                    prefix: prefix.clone(),
                    flavor: FileFlavor::Auxiliary,
                    crate_type: Some(crate_type.clone()),
                    should_replace_hyphens: true,
                });
            } else if target_triple.ends_with("windows-gnu") && suffix == ".dll" {
                // See https://cygwin.com/cygwin-ug-net/dll.html for more
                // information about GNU import libraries.
                // LD can link DLL directly, but LLD requires the import library.
                ret.push(FileType {
                    suffix: ".dll.a".to_string(),
                    prefix: "lib".to_string(),
                    flavor: FileFlavor::Auxiliary,
                    crate_type: Some(crate_type.clone()),
                    should_replace_hyphens: true,
                })
            }
        }

        if target_triple.starts_with("wasm32-") && crate_type == CrateType::Bin && suffix == ".js" {
            // emscripten binaries generate a .js file, which loads a .wasm
            // file.
            ret.push(FileType {
                suffix: ".wasm".to_string(),
                prefix: prefix.clone(),
                flavor: FileFlavor::Auxiliary,
                crate_type: Some(crate_type.clone()),
                // Name `foo-bar` will generate a `foo_bar.js` and
                // `foo_bar.wasm`. Cargo will translate the underscore and
                // copy `foo_bar.js` to `foo-bar.js`. However, the wasm
                // filename is embedded in the .js file with an underscore, so
                // it should not contain hyphens.
                should_replace_hyphens: true,
            });
            // And a map file for debugging. This is only emitted with debug=2
            // (-g4 for emcc).
            ret.push(FileType {
                suffix: ".wasm.map".to_string(),
                prefix: prefix.clone(),
                flavor: FileFlavor::DebugInfo,
                crate_type: Some(crate_type.clone()),
                should_replace_hyphens: true,
            });
        }

        // Handle separate debug files.
        let is_apple = target_triple.contains("-apple-");
        if matches!(
            crate_type,
            CrateType::Bin | CrateType::Dylib | CrateType::Cdylib | CrateType::ProcMacro
        ) {
            if is_apple {
                let suffix = if crate_type == CrateType::Bin {
                    ".dSYM".to_string()
                } else {
                    ".dylib.dSYM".to_string()
                };
                ret.push(FileType {
                    suffix,
                    prefix: prefix.clone(),
                    flavor: FileFlavor::DebugInfo,
                    crate_type: Some(crate_type),
                    // macOS tools like lldb use all sorts of magic to locate
                    // dSYM files. See https://lldb.llvm.org/use/symbols.html
                    // for some details. It seems like a `.dSYM` located next
                    // to the executable with the same name is one method. The
                    // dSYM should have the same hyphens as the executable for
                    // the names to match.
                    should_replace_hyphens: false,
                })
            } else if target_triple.ends_with("-msvc") {
                ret.push(FileType {
                    suffix: ".pdb".to_string(),
                    prefix: prefix.clone(),
                    flavor: FileFlavor::DebugInfo,
                    crate_type: Some(crate_type),
                    // The absolute path to the pdb file is embedded in the
                    // executable. If the exe/pdb pair is moved to another
                    // machine, then debuggers will look in the same directory
                    // of the exe with the original pdb filename. Since the
                    // original name contains underscores, they need to be
                    // preserved.
                    should_replace_hyphens: true,
                })
            }
        }

        Ok(Some(ret))
    }

    fn discover_crate_type(&self, crate_type: &CrateType) -> CargoResult<Option<(String, String)>> {
        let mut process = self.crate_type_process.clone();

        process.arg("--crate-type").arg(crate_type.as_str());

        let output = process.exec_with_output().with_context(|| {
            format!(
                "failed to run `rustc` to learn about crate-type {} information",
                crate_type
            )
        })?;

        let error = str::from_utf8(&output.stderr).unwrap();
        let output = str::from_utf8(&output.stdout).unwrap();
        parse_crate_type(crate_type, &process, output, error, &mut output.lines())
    }

    /// Returns all the file types generated by rustc for the given mode/target_kind.
    ///
    /// The first value is a Vec of file types generated, the second value is
    /// a list of CrateTypes that are not supported by the given target.
    pub fn rustc_outputs(
        &self,
        mode: CompileMode,
        target_kind: &TargetKind,
        target_triple: &str,
    ) -> CargoResult<(Vec<FileType>, Vec<CrateType>)> {
        match mode {
            CompileMode::Build => self.calc_rustc_outputs(target_kind, target_triple),
            CompileMode::Test | CompileMode::Bench => {
                match self.file_types(&CrateType::Bin, FileFlavor::Normal, target_triple)? {
                    Some(fts) => Ok((fts, Vec::new())),
                    None => Ok((Vec::new(), vec![CrateType::Bin])),
                }
            }
            CompileMode::Check { rustc_check, .. } => {
                let flav = if rustc_check {
                    FileType::new_rcheck()
                } else {
                    FileType::new_rmeta()
                };
                Ok((vec![flav], Vec::new()))
            }
            CompileMode::Doc { .. } | CompileMode::Doctest | CompileMode::RunCustomBuild => {
                panic!("asked for rustc output for non-rustc mode")
            }
        }
    }

    fn calc_rustc_outputs(
        &self,
        target_kind: &TargetKind,
        target_triple: &str,
    ) -> CargoResult<(Vec<FileType>, Vec<CrateType>)> {
        let mut unsupported = Vec::new();
        let mut result = Vec::new();
        let crate_types = target_kind.rustc_crate_types();
        for crate_type in &crate_types {
            let flavor = if crate_type.is_linkable() {
                FileFlavor::Linkable
            } else {
                FileFlavor::Normal
            };
            let file_types = self.file_types(crate_type, flavor, target_triple)?;
            match file_types {
                Some(types) => {
                    result.extend(types);
                }
                None => {
                    unsupported.push(crate_type.clone());
                }
            }
        }
        if !result.is_empty() && !crate_types.iter().any(|ct| ct.requires_upstream_objects()) {
            // Only add rmeta if pipelining.
            result.push(FileType::new_rmeta());
        }
        Ok((result, unsupported))
    }
}

/// Takes rustc output (using specialized command line args), and calculates the file prefix and
/// suffix for the given crate type, or returns `None` if the type is not supported. (e.g., for a
/// Rust library like `libcargo.rlib`, we have prefix "lib" and suffix "rlib").
///
/// The caller needs to ensure that the lines object is at the correct line for the given crate
/// type: this is not checked.
///
/// This function can not handle more than one file per type (with wasm32-unknown-emscripten, there
/// are two files for bin (`.wasm` and `.js`)).
fn parse_crate_type(
    crate_type: &CrateType,
    cmd: &ProcessBuilder,
    output: &str,
    error: &str,
    lines: &mut str::Lines<'_>,
) -> CargoResult<Option<(String, String)>> {
    let not_supported = error.lines().any(|line| {
        (line.contains("unsupported crate type") || line.contains("unknown crate type"))
            && line.contains(&format!("crate type `{}`", crate_type))
    });
    if not_supported {
        return Ok(None);
    }
    let line = match lines.next() {
        Some(line) => line,
        None => anyhow::bail!(
            "malformed output when learning about crate-type {} information\n{}",
            crate_type,
            output_err_info(cmd, output, error)
        ),
    };
    let mut parts = line.trim().split("___");
    let prefix = parts.next().unwrap();
    let suffix = match parts.next() {
        Some(part) => part,
        None => anyhow::bail!(
            "output of --print=file-names has changed in the compiler, cannot parse\n{}",
            output_err_info(cmd, output, error)
        ),
    };

    Ok(Some((prefix.to_string(), suffix.to_string())))
}

/// Helper for creating an error message when parsing rustc output fails.
fn output_err_info(cmd: &ProcessBuilder, stdout: &str, stderr: &str) -> String {
    let mut result = format!("command was: {}\n", cmd);
    if !stdout.is_empty() {
        result.push_str("\n--- stdout\n");
        result.push_str(stdout);
    }
    if !stderr.is_empty() {
        result.push_str("\n--- stderr\n");
        result.push_str(stderr);
    }
    if stdout.is_empty() && stderr.is_empty() {
        result.push_str("(no output received)");
    }
    result
}

/// Acquire extra flags to pass to the compiler from various locations.
///
/// The locations are:
///
///  - the `RUSTFLAGS` environment variable
///
/// then if this was not found
///
///  - `target.*.rustflags` from the config (.cargo/config)
///  - `target.cfg(..).rustflags` from the config
///
/// then if neither of these were found
///
///  - `build.rustflags` from the config
///
/// Note that if a `target` is specified, no args will be passed to host code (plugins, build
/// scripts, ...), even if it is the same as the target.
fn env_args(
    config: &Config,
    requested_kinds: &[CompileKind],
    host_triple: &str,
    target_cfg: Option<&[Cfg]>,
    kind: CompileKind,
    name: &str,
) -> CargoResult<Vec<String>> {
    // We *want* to apply RUSTFLAGS only to builds for the
    // requested target architecture, and not to things like build
    // scripts and plugins, which may be for an entirely different
    // architecture. Cargo's present architecture makes it quite
    // hard to only apply flags to things that are not build
    // scripts and plugins though, so we do something more hacky
    // instead to avoid applying the same RUSTFLAGS to multiple targets
    // arches:
    //
    // 1) If --target is not specified we just apply RUSTFLAGS to
    // all builds; they are all going to have the same target.
    //
    // 2) If --target *is* specified then we only apply RUSTFLAGS
    // to compilation units with the Target kind, which indicates
    // it was chosen by the --target flag.
    //
    // This means that, e.g., even if the specified --target is the
    // same as the host, build scripts in plugins won't get
    // RUSTFLAGS.
    if requested_kinds != [CompileKind::Host] && kind.is_host() {
        // This is probably a build script or plugin and we're
        // compiling with --target. In this scenario there are
        // no rustflags we can apply.
        return Ok(Vec::new());
    }

    // First try RUSTFLAGS from the environment
    if let Ok(a) = env::var(name) {
        let args = a
            .split(' ')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Ok(args.collect());
    }

    let mut rustflags = Vec::new();

    let name = name
        .chars()
        .flat_map(|c| c.to_lowercase())
        .collect::<String>();
    // Then the target.*.rustflags value...
    let target = match &kind {
        CompileKind::Host => host_triple,
        CompileKind::Target(target) => target.short_name(),
    };
    let key = format!("target.{}.{}", target, name);
    if let Some(args) = config.get::<Option<StringList>>(&key)? {
        rustflags.extend(args.as_slice().iter().cloned());
    }
    // ...including target.'cfg(...)'.rustflags
    if let Some(target_cfg) = target_cfg {
        config
            .target_cfgs()?
            .iter()
            .filter_map(|(key, cfg)| {
                cfg.rustflags
                    .as_ref()
                    .map(|rustflags| (key, &rustflags.val))
            })
            .filter(|(key, _rustflags)| CfgExpr::matches_key(key, target_cfg))
            .for_each(|(_key, cfg_rustflags)| {
                rustflags.extend(cfg_rustflags.as_slice().iter().cloned());
            });
    }

    if !rustflags.is_empty() {
        return Ok(rustflags);
    }

    // Then the `build.rustflags` value.
    let build = config.build_config()?;
    let list = if name == "rustflags" {
        &build.rustflags
    } else {
        &build.rustdocflags
    };
    if let Some(list) = list {
        return Ok(list.as_slice().to_vec());
    }

    Ok(Vec::new())
}

/// Collection of information about `rustc` and the host and target.
pub struct RustcTargetData<'cfg> {
    /// Information about `rustc` itself.
    pub rustc: Rustc,

    /// Config
    config: &'cfg Config,
    requested_kinds: Vec<CompileKind>,

    /// Build information for the "host", which is information about when
    /// `rustc` is invoked without a `--target` flag. This is used for
    /// procedural macros, build scripts, etc.
    host_config: TargetConfig,
    host_info: TargetInfo,

    /// Build information for targets that we're building for. This will be
    /// empty if the `--target` flag is not passed.
    target_config: HashMap<CompileTarget, TargetConfig>,
    target_info: HashMap<CompileTarget, TargetInfo>,
}

impl<'cfg> RustcTargetData<'cfg> {
    pub fn new(
        ws: &Workspace<'cfg>,
        requested_kinds: &[CompileKind],
    ) -> CargoResult<RustcTargetData<'cfg>> {
        let config = ws.config();
        let rustc = config.load_global_rustc(Some(ws))?;
        let host_config = config.target_cfg_triple(&rustc.host)?;
        let host_info = TargetInfo::new(config, requested_kinds, &rustc, CompileKind::Host)?;
        let mut target_config = HashMap::new();
        let mut target_info = HashMap::new();

        // This is a hack. The unit_dependency graph builder "pretends" that
        // `CompileKind::Host` is `CompileKind::Target(host)` if the
        // `--target` flag is not specified. Since the unit_dependency code
        // needs access to the target config data, create a copy so that it
        // can be found. See `rebuild_unit_graph_shared` for why this is done.
        if requested_kinds.iter().any(CompileKind::is_host) {
            let ct = CompileTarget::new(&rustc.host)?;
            target_info.insert(ct, host_info.clone());
            target_config.insert(ct, host_config.clone());
        }

        let mut res = RustcTargetData {
            rustc,
            config,
            requested_kinds: requested_kinds.into(),
            host_config,
            host_info,
            target_config,
            target_info,
        };

        // Get all kinds we currently know about.
        //
        // For now, targets can only ever come from the root workspace
        // units as artifact dependencies are not a thing yet, so this
        // correctly represents all the kinds that can happen. When we
        // have artifact dependencies or other ways for targets to
        // appear at places that are not the root units, we may have
        // to revisit this.
        let all_kinds = requested_kinds
            .iter()
            .copied()
            .chain(ws.members().flat_map(|p| {
                p.manifest()
                    .default_kind()
                    .into_iter()
                    .chain(p.manifest().forced_kind())
            }));
        for kind in all_kinds {
            if let CompileKind::Target(target) = kind {
                if !res.target_config.contains_key(&target) {
                    res.target_config
                        .insert(target, res.config.target_cfg_triple(target.short_name())?);
                }
                if !res.target_info.contains_key(&target) {
                    res.target_info.insert(
                        target,
                        TargetInfo::new(res.config, &res.requested_kinds, &res.rustc, kind)?,
                    );
                }
            }
        }

        Ok(res)
    }

    /// Returns a "short" name for the given kind, suitable for keying off
    /// configuration in Cargo or presenting to users.
    pub fn short_name<'a>(&'a self, kind: &'a CompileKind) -> &'a str {
        match kind {
            CompileKind::Host => &self.rustc.host,
            CompileKind::Target(target) => target.short_name(),
        }
    }

    /// Whether a dependency should be compiled for the host or target platform,
    /// specified by `CompileKind`.
    pub fn dep_platform_activated(&self, dep: &Dependency, kind: CompileKind) -> bool {
        // If this dependency is only available for certain platforms,
        // make sure we're only enabling it for that platform.
        let platform = match dep.platform() {
            Some(p) => p,
            None => return true,
        };
        let name = self.short_name(&kind);
        platform.matches(name, self.cfg(kind))
    }

    /// Gets the list of `cfg`s printed out from the compiler for the specified kind.
    pub fn cfg(&self, kind: CompileKind) -> &[Cfg] {
        self.info(kind).cfg()
    }

    /// Information about the given target platform, learned by querying rustc.
    pub fn info(&self, kind: CompileKind) -> &TargetInfo {
        match kind {
            CompileKind::Host => &self.host_info,
            CompileKind::Target(s) => &self.target_info[&s],
        }
    }

    /// Gets the target configuration for a particular host or target.
    pub fn target_config(&self, kind: CompileKind) -> &TargetConfig {
        match kind {
            CompileKind::Host => &self.host_config,
            CompileKind::Target(s) => &self.target_config[&s],
        }
    }

    /// If a build script is overridden, this returns the `BuildOutput` to use.
    ///
    /// `lib_name` is the `links` library name and `kind` is whether it is for
    /// Host or Target.
    pub fn script_override(&self, lib_name: &str, kind: CompileKind) -> Option<&BuildOutput> {
        self.target_config(kind).links_overrides.get(lib_name)
    }
}

/// Structure used to deal with Rustdoc fingerprinting
#[derive(Debug, Serialize, Deserialize)]
pub struct RustDocFingerprint {
    pub rustc_vv: String,
}

impl RustDocFingerprint {
    /// This function checks whether the latest version of `Rustc` used to compile this
    /// `Workspace`'s docs was the same as the one is currently being used in this `cargo doc`
    /// call.
    ///
    /// In case it's not, it takes care of removing the `doc/` folder as well as overwriting
    /// the rustdoc fingerprint info in order to guarantee that we won't end up with mixed
    /// versions of the `js/html/css` files that `rustdoc` autogenerates which do not have
    /// any versioning.
    pub fn check_rustdoc_fingerprint(cx: &Context<'_, '_>) -> CargoResult<()> {
        if cx.bcx.config.cli_unstable().skip_rustdoc_fingerprint {
            return Ok(());
        }
        let actual_rustdoc_target_data = RustDocFingerprint {
            rustc_vv: cx.bcx.rustc().verbose_version.clone(),
        };

        let fingerprint_path = cx.files().host_root().join(".rustdoc_fingerprint.json");
        let write_fingerprint = || -> CargoResult<()> {
            paths::write(
                &fingerprint_path,
                serde_json::to_string(&actual_rustdoc_target_data)?,
            )
        };
        let rustdoc_data = match paths::read(&fingerprint_path) {
            Ok(rustdoc_data) => rustdoc_data,
            // If the fingerprint does not exist, do not clear out the doc
            // directories. Otherwise this ran into problems where projects
            // like rustbuild were creating the doc directory before running
            // `cargo doc` in a way that deleting it would break it.
            Err(_) => return write_fingerprint(),
        };
        match serde_json::from_str::<RustDocFingerprint>(&rustdoc_data) {
            Ok(fingerprint) => {
                if fingerprint.rustc_vv == actual_rustdoc_target_data.rustc_vv {
                    return Ok(());
                } else {
                    log::debug!(
                        "doc fingerprint changed:\noriginal:\n{}\nnew:\n{}",
                        fingerprint.rustc_vv,
                        actual_rustdoc_target_data.rustc_vv
                    );
                }
            }
            Err(e) => {
                log::debug!("could not deserialize {:?}: {}", fingerprint_path, e);
            }
        };
        // Fingerprint does not match, delete the doc directories and write a new fingerprint.
        log::debug!(
            "fingerprint {:?} mismatch, clearing doc directories",
            fingerprint_path
        );
        cx.bcx
            .all_kinds
            .iter()
            .map(|kind| cx.files().layout(*kind).doc())
            .filter(|path| path.exists())
            .try_for_each(|path| clean_doc(path))?;
        write_fingerprint()?;
        return Ok(());

        fn clean_doc(path: &Path) -> CargoResult<()> {
            let entries = path
                .read_dir()
                .with_context(|| format!("failed to read directory `{}`", path.display()))?;
            for entry in entries {
                let entry = entry?;
                // Don't remove hidden files. Rustdoc does not create them,
                // but the user might have.
                if entry
                    .file_name()
                    .to_str()
                    .map_or(false, |name| name.starts_with('.'))
                {
                    continue;
                }
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    paths::remove_dir_all(path)?;
                } else {
                    paths::remove_file(path)?;
                }
            }
            Ok(())
        }
    }
}
