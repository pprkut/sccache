// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The Go compiler (`go tool compile`), reached through
//! `go build -toolexec sccache`.
//!
//! With `-toolexec` the go command runs every tool of the toolchain through
//! sccache. Only `compile` is cached and distributed; the other tools are
//! recognised so that they run as they are.

use crate::cache::{FileObjectSource, Storage};
use crate::compiler::{
    CCompileCommand, CacheControl, Cacheable, ColorMode, Compilation, CompileCommand, Compiler,
    CompilerArguments, CompilerHasher, CompilerKind, HashResult, Language, SingleCompileCommand,
};
#[cfg(feature = "dist-client")]
use crate::compiler::{DistPackagers, NoopOutputsRewriter};
use crate::dist;
#[cfg(feature = "dist-client")]
use crate::dist::pkg;
use crate::errors::*;
use crate::mock_command::{CommandCreatorSync, RunCommand};
use crate::util::{Digest, run_input_output};
use async_trait::async_trait;
use fs_err as fs;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bump to invalidate every cached Go compilation.
const CACHE_VERSION: &[u8] = b"1";

/// `GO*` environment variables that the go command reads and the compiler does
/// not. All other `GO*` variables are part of the cache key.
const GO_COMMAND_ENV: &[&str] = &[
    "GOAUTH",
    "GOBIN",
    "GOCACHE",
    "GOCACHEPROG",
    "GOENV",
    "GOFLAGS",
    "GOINSECURE",
    "GOMODCACHE",
    "GONOPROXY",
    "GONOSUMDB",
    "GOPATH",
    "GOPRIVATE",
    "GOPROXY",
    "GOSUMDB",
    "GOTELEMETRY",
    "GOTELEMETRYDIR",
    "GOTMPDIR",
    "GOTOOLCHAIN",
    "GOVCS",
    "GOWORK",
];

/// Variables that make the compiler write files sccache does not know about.
const UNCACHEABLE_ENV: &[&str] = &["GOSSAFUNC", "GOSSADIR", "GSHS_LOGFILE"];

const ARCHIVE_MAGIC: &[u8] = b"!<arch>\n";
const ARCHIVE_HEADER_LEN: usize = 60;
const EXPORT_DATA_MEMBER: &[u8] = b"__.PKGDEF";

/// A tool of the Go toolchain.
#[derive(Debug, Clone)]
pub struct Go {
    executable: PathBuf,
    executable_digest: String,
    /// `compile`, `asm`, `link`, ...
    tool: String,
    /// What the tool prints for `-V=full`.
    version: String,
}

/// Whether `executable` is worth asking for its Go tool version.
pub fn is_go_tool_like(executable: &Path, env_vars: &[(OsString, OsString)]) -> bool {
    // Set by the go command for everything it runs through -toolexec.
    if env_vars.iter().any(|(k, _)| k == "TOOLEXEC_IMPORTPATH") {
        return true;
    }
    // $GOROOT/pkg/tool/$GOOS_$GOARCH/$tool
    let mut dirs = executable.parent().into_iter().flat_map(Path::ancestors);
    dirs.next();
    let tool = dirs.next().and_then(Path::file_name);
    let pkg = dirs.next().and_then(Path::file_name);
    tool == Some(OsStr::new("tool")) && pkg == Some(OsStr::new("pkg"))
}

impl Go {
    pub async fn new<T>(
        creator: T,
        executable: PathBuf,
        env_vars: &[(OsString, OsString)],
        pool: &tokio::runtime::Handle,
    ) -> Result<Go>
    where
        T: CommandCreatorSync,
    {
        let tool = executable
            .file_stem()
            .and_then(OsStr::to_str)
            .context("Go tool without a name")?
            .to_owned();
        let mut child = creator.clone().new_command_sync(&executable);
        child.env_clear().envs(env_vars.to_vec()).args(&["-V=full"]);
        let output = run_input_output(child, None).await?;
        let version = String::from_utf8(output.stdout)?.trim().to_owned();
        // `compile version go1.24.7`
        let is_go_tool = version
            .strip_prefix(&tool)
            .is_some_and(|rest| rest.starts_with(" version "));
        if !is_go_tool {
            bail!("{} is not a Go tool", executable.display());
        }
        let executable_digest = Digest::file(&executable, pool).await?;
        Ok(Go {
            executable,
            executable_digest,
            tool,
            version,
        })
    }
}

impl<T> Compiler<T> for Go
where
    T: CommandCreatorSync,
{
    fn kind(&self) -> CompilerKind {
        CompilerKind::Go
    }

    #[cfg(feature = "dist-client")]
    fn get_toolchain_packager(&self) -> Box<dyn pkg::ToolchainPackager> {
        Box::new(GoToolchainPackager {
            executable: self.executable.clone(),
        })
    }

    fn parse_arguments(
        &self,
        arguments: &[OsString],
        _cwd: &Path,
        env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<Box<dyn CompilerHasher<T> + 'static>> {
        if self.tool != "compile" {
            return CompilerArguments::NotCompilation;
        }
        if let Some((name, _)) = env_vars
            .iter()
            .find(|(k, _)| UNCACHEABLE_ENV.iter().any(|name| k == name))
        {
            return CompilerArguments::CannotCache(
                "environment variable",
                Some(name.to_string_lossy().into_owned()),
            );
        }
        match parse_arguments(arguments) {
            CompilerArguments::Ok(parsed_args) => CompilerArguments::Ok(Box::new(GoHasher {
                executable: self.executable.clone(),
                executable_digest: self.executable_digest.clone(),
                version: self.version.clone(),
                parsed_args,
            })),
            CompilerArguments::CannotCache(why, extra_info) => {
                CompilerArguments::CannotCache(why, extra_info)
            }
            CompilerArguments::NotCompilation => CompilerArguments::NotCompilation,
        }
    }

    fn box_clone(&self) -> Box<dyn Compiler<T>> {
        Box::new((*self).clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArguments {
    arguments: Vec<OsString>,
    /// The flags that go into the cache key as they are, `name` or `name=value`.
    hashed_flags: Vec<String>,
    output: PathBuf,
    asmhdr: Option<PathBuf>,
    linkobj: Option<PathBuf>,
    importcfg: Option<PathBuf>,
    embedcfg: Option<PathBuf>,
    symabis: Option<PathBuf>,
    trimpath: String,
    package: String,
    sources: Vec<PathBuf>,
}

enum Flag {
    /// Takes no value and changes the output.
    Bool,
    /// Takes a value and changes the output.
    Value,
    /// Takes a value that does not change the output.
    IgnoredValue,
    Output,
    AsmHdr,
    LinkObj,
    ImportCfg,
    EmbedCfg,
    SymAbis,
    TrimPath,
    Package,
    Version,
    /// Reads or writes files sccache does not track, prints debug output, or
    /// is simply unknown: such a compile runs uncached.
    Unsupported,
}

fn flag(name: &str) -> Flag {
    match name {
        "+" | "B" | "C" | "N" | "l" | "asan" | "complete" | "dwarf" | "dwarfbasentries"
        | "dwarflocationlists" | "e" | "errorurl" | "msan" | "nolocalimports" | "pack" | "race"
        | "shared" | "smallframes" | "std" | "wb" => Flag::Bool,
        "buildid" | "gendwarfinl" | "goversion" | "installsuffix" | "lang" | "spectre" => {
            Flag::Value
        }
        // The number of threads the backend uses.
        "c" => Flag::IgnoredValue,
        "o" => Flag::Output,
        "asmhdr" => Flag::AsmHdr,
        "linkobj" => Flag::LinkObj,
        "importcfg" => Flag::ImportCfg,
        "embedcfg" => Flag::EmbedCfg,
        "symabis" => Flag::SymAbis,
        "trimpath" => Flag::TrimPath,
        "p" => Flag::Package,
        "V" => Flag::Version,
        _ => Flag::Unsupported,
    }
}

fn parse_arguments(arguments: &[OsString]) -> CompilerArguments<ParsedArguments> {
    let mut hashed_flags = vec![];
    let mut output = None;
    let mut asmhdr = None;
    let mut linkobj = None;
    let mut importcfg = None;
    let mut embedcfg = None;
    let mut symabis = None;
    let mut trimpath = String::new();
    let mut package = String::new();
    let mut sources = vec![];

    let mut args = arguments.iter().map(|arg| arg.to_str());
    let mut flags_done = false;
    while let Some(arg) = args.next() {
        let Some(arg) = arg else {
            cannot_cache!("non-UTF-8 argument");
        };
        // The flag package: `-flag`, `--flag`, `-flag=value`, `-flag value`,
        // flags end at `--` or at the first argument that is not one.
        let name_and_value = arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .filter(|rest| !flags_done && !rest.is_empty());
        let Some(name_and_value) = name_and_value else {
            if !flags_done && arg == "--" {
                flags_done = true;
                continue;
            }
            flags_done = true;
            if !arg.ends_with(".go") {
                cannot_cache!("source file", arg.to_owned());
            }
            sources.push(PathBuf::from(arg));
            continue;
        };
        let (name, inline_value) = match name_and_value.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (name_and_value, None),
        };
        let kind = flag(name);
        match kind {
            Flag::Version => return CompilerArguments::NotCompilation,
            Flag::Unsupported => cannot_cache!("compiler flag", format!("-{name}")),
            Flag::Bool => {
                hashed_flags.push(name_and_value.to_owned());
                continue;
            }
            _ => {}
        }
        let value = match inline_value {
            Some(value) => value,
            None => match args.next().flatten() {
                Some(value) => value,
                None => cannot_cache!("flag without a value", format!("-{name}")),
            },
        };
        match kind {
            Flag::Value => hashed_flags.push(format!("{name}={value}")),
            Flag::IgnoredValue => {}
            Flag::Output => output = Some(PathBuf::from(value)),
            Flag::AsmHdr => asmhdr = Some(PathBuf::from(value)),
            Flag::LinkObj => linkobj = Some(PathBuf::from(value)),
            Flag::ImportCfg => importcfg = Some(PathBuf::from(value)),
            Flag::EmbedCfg => embedcfg = Some(PathBuf::from(value)),
            Flag::SymAbis => symabis = Some(PathBuf::from(value)),
            Flag::TrimPath => value.clone_into(&mut trimpath),
            Flag::Package => value.clone_into(&mut package),
            Flag::Bool | Flag::Version | Flag::Unsupported => unreachable!(),
        }
    }

    let Some(output) = output else {
        cannot_cache!("no output file");
    };
    if sources.is_empty() {
        cannot_cache!("no source file");
    }
    CompilerArguments::Ok(ParsedArguments {
        arguments: arguments.to_vec(),
        hashed_flags,
        output,
        asmhdr,
        linkobj,
        importcfg,
        embedcfg,
        symabis,
        trimpath,
        package,
        sources,
    })
}

/// A file the compiler reads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Input {
    path: PathBuf,
    /// A package archive, of which the compiler only reads the export data.
    export_data_only: bool,
}

/// The part of the package archive at `path` that a compile importing the
/// package reads: the archive header and its first member, the export data.
/// The rest is the object code, which only the linker needs. A file that does
/// not look like that is returned whole.
fn read_export_data(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut data = vec![0; ARCHIVE_MAGIC.len() + ARCHIVE_HEADER_LEN];
    let mut read = 0;
    while read < data.len() {
        match file.read(&mut data[read..])? {
            0 => break,
            n => read += n,
        }
    }
    data.truncate(read);

    let member_len = (|| {
        let header = data.strip_prefix(ARCHIVE_MAGIC)?;
        if header.len() != ARCHIVE_HEADER_LEN || !header.starts_with(EXPORT_DATA_MEMBER) {
            return None;
        }
        let size = std::str::from_utf8(&header[48..58]).ok()?;
        size.trim().parse::<u64>().ok()
    })();
    match member_len {
        // Archive members are padded to an even length.
        Some(len) => {
            file.take(len + len % 2).read_to_end(&mut data)?;
        }
        None => {
            file.read_to_end(&mut data)?;
        }
    }
    Ok(data)
}

/// `hasPathPrefix` of cmd/internal/objabi.
fn has_path_prefix(path: &str, prefix: &str) -> bool {
    let fold = |b: u8| match b {
        b'\\' => b'/',
        b => b.to_ascii_lowercase(),
    };
    let (path, prefix) = (path.as_bytes(), prefix.as_bytes());
    path.len() >= prefix.len()
        && path.iter().zip(prefix).all(|(a, b)| fold(*a) == fold(*b))
        && matches!(path.get(prefix.len()), None | Some(b'/' | b'\\'))
}

/// `ApplyRewrites` of cmd/internal/objabi: how `-trimpath` changes a path.
fn apply_rewrites(path: &str, rewrites: &str) -> Option<String> {
    rewrites.split(';').find_map(|rewrite| {
        let (prefix, replace) = rewrite.rsplit_once("=>").unwrap_or((rewrite, ""));
        if prefix.is_empty() || !has_path_prefix(path, prefix) {
            return None;
        }
        let rest = &path[prefix.len()..];
        Some(if rest.is_empty() {
            replace.to_owned()
        } else if replace.is_empty() {
            rest[1..].to_owned()
        } else {
            format!("{replace}{rest}")
        })
    })
}

/// The name the compiler records for `path`, as `AbsFile` of
/// cmd/internal/objabi computes it. Two paths with the same recorded name
/// are the same to the compiler; the reverse does not have to hold.
fn recorded_path(path: &Path, trimpath: &str, goroot: Option<&str>) -> String {
    let path = path.to_string_lossy();
    if let Some(rewritten) = apply_rewrites(&path, trimpath) {
        return rewritten;
    }
    match goroot {
        Some(goroot) if !goroot.is_empty() && has_path_prefix(&path, goroot) => {
            format!("$GOROOT{}", &path[goroot.len()..])
        }
        _ => path.into_owned(),
    }
}

fn hashed_env(env_vars: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    let mut hashed = env_vars
        .iter()
        .filter(|(k, _)| {
            k.to_str()
                .is_some_and(|k| k.starts_with("GO") && !GO_COMMAND_ENV.contains(&k))
        })
        .cloned()
        .collect::<Vec<_>>();
    hashed.sort();
    hashed
}

fn digest_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path)?;
    Digest::reader_sync(file)
}

fn digest_bytes(bytes: &[u8]) -> String {
    let mut digest = Digest::new();
    digest.update(bytes);
    digest.finish()
}

/// Compute the cache key of a compilation and list the files it reads.
fn hash_key(
    executable_digest: &str,
    version: &str,
    parsed_args: &ParsedArguments,
    cwd: &Path,
    env_vars: &[(OsString, OsString)],
) -> Result<(String, Vec<Input>)> {
    let mut m = Digest::new();
    let mut inputs = vec![];
    let mut field = |name: &str, value: &[u8]| {
        m.delimiter(name.as_bytes());
        m.update(value);
    };

    field("version", CACHE_VERSION);
    field("compiler", executable_digest.as_bytes());
    field("compiler version", version.as_bytes());
    field("package", parsed_args.package.as_bytes());
    for flag in &parsed_args.hashed_flags {
        field("flag", flag.as_bytes());
    }
    for (name, value) in hashed_env(env_vars) {
        field("env", name.as_encoded_bytes());
        field("env value", value.as_encoded_bytes());
    }

    // The compiler records source positions after applying -trimpath, so the
    // key holds the trimmed names and not the paths: that is what lets a
    // package built in another directory, or in another of the go command's
    // work directories, reuse a result.
    let goroot = env_vars
        .iter()
        .find(|(k, _)| k == "GOROOT")
        .and_then(|(_, v)| v.to_str());
    let recorded = |path: &Path| recorded_path(path, &parsed_args.trimpath, goroot);
    let mut has_line_directive = false;
    for source in &parsed_args.sources {
        let source = cwd.join(source);
        let contents = fs::read(&source)?;
        has_line_directive |= [&b"//line "[..], b"/*line "]
            .iter()
            .any(|directive| memchr::memmem::find(&contents, directive).is_some());
        field("source", recorded(&source).as_bytes());
        field("source contents", digest_bytes(&contents).as_bytes());
        inputs.push(Input {
            path: source,
            export_data_only: false,
        });
    }
    // A line directive can name any path, which -trimpath then rewrites as
    // well. Generated files have them, cgo's in particular, and those name
    // the work directory.
    if has_line_directive {
        field("trimpath", parsed_args.trimpath.as_bytes());
    }
    // Apart from the source files, which may be named relative to it, the
    // compiler resolves relative line directives and local imports against
    // its working directory. With -trimpath that is wherever the go command
    // was started, so it is left out when it cannot matter.
    let local_imports = !parsed_args
        .hashed_flags
        .iter()
        .any(|flag| flag == "nolocalimports");
    if has_line_directive || local_imports {
        field("cwd", recorded(cwd).as_bytes());
    }

    if let Some(importcfg) = &parsed_args.importcfg {
        let importcfg = cwd.join(importcfg);
        for line in fs::read_to_string(&importcfg)?.lines() {
            let package_file = line
                .strip_prefix("packagefile ")
                .and_then(|rest| rest.split_once('='));
            match package_file {
                Some((package, path)) => {
                    let path = cwd.join(path);
                    field("import", package.as_bytes());
                    field(
                        "import export data",
                        digest_bytes(&read_export_data(&path)?).as_bytes(),
                    );
                    inputs.push(Input {
                        path,
                        export_data_only: true,
                    });
                }
                None => field("importcfg", line.as_bytes()),
            }
        }
        inputs.push(Input {
            path: importcfg,
            export_data_only: false,
        });
    }

    if let Some(embedcfg) = &parsed_args.embedcfg {
        #[derive(serde::Deserialize)]
        struct EmbedCfg {
            #[serde(rename = "Patterns", default)]
            patterns: BTreeMap<String, Vec<String>>,
            #[serde(rename = "Files", default)]
            files: BTreeMap<String, PathBuf>,
        }
        let embedcfg = cwd.join(embedcfg);
        let cfg: EmbedCfg = serde_json::from_slice(&fs::read(&embedcfg)?)
            .with_context(|| format!("cannot parse {}", embedcfg.display()))?;
        for (pattern, names) in &cfg.patterns {
            field("embed pattern", pattern.as_bytes());
            for name in names {
                field("embed pattern file", name.as_bytes());
            }
        }
        for (name, path) in cfg.files {
            let path = cwd.join(path);
            field("embed file", name.as_bytes());
            field("embed file contents", digest_file(&path)?.as_bytes());
            inputs.push(Input {
                path,
                export_data_only: false,
            });
        }
        inputs.push(Input {
            path: embedcfg,
            export_data_only: false,
        });
    }

    if let Some(symabis) = &parsed_args.symabis {
        let symabis = cwd.join(symabis);
        field("symabis", digest_file(&symabis)?.as_bytes());
        inputs.push(Input {
            path: symabis,
            export_data_only: false,
        });
    }

    inputs.sort();
    inputs.dedup();
    Ok((m.finish(), inputs))
}

/// With GOCACHEPROG the go command stores every result in an external cache
/// already, sccache's own included, so storing it here as well only costs.
fn cacheable(env_vars: &[(OsString, OsString)]) -> Cacheable {
    let has_cache_program = env_vars
        .iter()
        .any(|(k, v)| k == "GOCACHEPROG" && !v.is_empty());
    match has_cache_program {
        true => Cacheable::No,
        false => Cacheable::Yes,
    }
}

#[derive(Debug, Clone)]
pub struct GoHasher {
    executable: PathBuf,
    executable_digest: String,
    version: String,
    parsed_args: ParsedArguments,
}

#[async_trait]
impl<T> CompilerHasher<T> for GoHasher
where
    T: CommandCreatorSync,
{
    async fn generate_hash_key(
        &mut self,
        _creator: &T,
        cwd: PathBuf,
        env_vars: Vec<(OsString, OsString)>,
        _may_dist: bool,
        pool: &tokio::runtime::Handle,
        _rewrite_includes_only: bool,
        _storage: Arc<dyn Storage>,
        _cache_control: CacheControl,
    ) -> Result<HashResult<T>> {
        let me = self.clone();
        let (key, inputs, cwd, env_vars) = pool
            .spawn_blocking(move || {
                let (key, inputs) = hash_key(
                    &me.executable_digest,
                    &me.version,
                    &me.parsed_args,
                    &cwd,
                    &env_vars,
                )?;
                anyhow::Ok((key, inputs, cwd, env_vars))
            })
            .await??;

        let cacheable = cacheable(&env_vars);
        let outputs = [
            ("obj", Some(&self.parsed_args.output)),
            ("asmhdr", self.parsed_args.asmhdr.as_ref()),
            ("linkobj", self.parsed_args.linkobj.as_ref()),
        ]
        .into_iter()
        .filter_map(|(key, path)| {
            Some(FileObjectSource {
                key: key.to_owned(),
                path: path?.clone(),
                optional: false,
            })
        })
        .collect();
        // The executable path is part of the key because the packaged
        // toolchain holds the compiler at that path.
        let weak_toolchain_key = format!(
            "{}-{}",
            self.executable.to_string_lossy(),
            self.executable_digest
        );
        Ok(HashResult {
            key,
            compilation: Box::new(GoCompilation {
                executable: self.executable.clone(),
                arguments: self.parsed_args.arguments.clone(),
                dist_env_vars: hashed_env(&env_vars),
                env_vars,
                cwd,
                outputs,
                inputs,
                cacheable,
            }),
            weak_toolchain_key,
        })
    }

    fn color_mode(&self) -> ColorMode {
        ColorMode::Auto
    }

    fn output_pretty(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.parsed_args.package)
    }

    fn box_clone(&self) -> Box<dyn CompilerHasher<T>> {
        Box::new((*self).clone())
    }

    fn language(&self) -> Language {
        Language::Go
    }
}

#[allow(unused)]
struct GoCompilation {
    executable: PathBuf,
    arguments: Vec<OsString>,
    env_vars: Vec<(OsString, OsString)>,
    /// The part of the environment in the cache key, which is all a build
    /// server gets to see.
    dist_env_vars: Vec<(OsString, OsString)>,
    cwd: PathBuf,
    outputs: Vec<FileObjectSource>,
    inputs: Vec<Input>,
    cacheable: Cacheable,
}

impl<T> Compilation<T> for GoCompilation
where
    T: CommandCreatorSync,
{
    fn generate_compile_commands(
        &self,
        path_transformer: &mut dist::PathTransformer,
        _rewrite_includes_only: bool,
    ) -> Result<(
        Box<dyn CompileCommand<T>>,
        Option<dist::CompileCommand>,
        Cacheable,
    )> {
        let command = SingleCompileCommand {
            executable: self.executable.clone(),
            arguments: self.arguments.clone(),
            env_vars: self.env_vars.clone(),
            cwd: self.cwd.clone(),
            share_jobserver: false,
        };

        #[cfg(not(feature = "dist-client"))]
        let dist_command = {
            let _ = path_transformer;
            None
        };
        // The arguments and the import configuration name files by their
        // local path, so a build server has to see them at the same place.
        // That rules out clients whose paths need translating.
        #[cfg(feature = "dist-client")]
        let dist_command = (|| {
            if cfg!(windows) {
                return None;
            }
            Some(dist::CompileCommand {
                executable: path_transformer.as_dist(&self.executable)?,
                arguments: dist::osstrings_to_strings(&self.arguments)?,
                env_vars: dist::osstring_tuples_to_strings(&self.dist_env_vars)?,
                cwd: path_transformer.as_dist_abs(&self.cwd)?,
            })
        })();

        Ok((CCompileCommand::new(command), dist_command, self.cacheable))
    }

    #[cfg(feature = "dist-client")]
    fn into_dist_packagers(
        self: Box<Self>,
        path_transformer: dist::PathTransformer,
    ) -> Result<DistPackagers> {
        let GoCompilation {
            executable, inputs, ..
        } = *self;
        Ok((
            Box::new(GoInputsPackager {
                inputs,
                path_transformer,
            }),
            Box::new(GoToolchainPackager { executable }),
            Box::new(NoopOutputsRewriter),
        ))
    }

    fn outputs<'a>(&'a self) -> Box<dyn Iterator<Item = FileObjectSource> + 'a> {
        Box::new(self.outputs.iter().cloned())
    }
}

#[cfg(feature = "dist-client")]
struct GoInputsPackager {
    inputs: Vec<Input>,
    path_transformer: dist::PathTransformer,
}

#[cfg(feature = "dist-client")]
impl pkg::InputsPackager for GoInputsPackager {
    fn write_inputs(self: Box<Self>, wtr: &mut dyn io::Write) -> Result<dist::PathTransformer> {
        let GoInputsPackager {
            inputs,
            mut path_transformer,
        } = *self;
        let mut builder = tar::Builder::new(wtr);
        for input in inputs {
            let dist_path = path_transformer
                .as_dist(&input.path)
                .with_context(|| format!("unable to transform {}", input.path.display()))?;
            let mut header = pkg::make_tar_header(&input.path, &dist_path)?;
            if input.export_data_only {
                let export_data = read_export_data(&input.path)?;
                header.set_size(export_data.len() as u64);
                header.set_cksum();
                builder.append(&header, export_data.as_slice())?;
            } else {
                header.set_cksum();
                builder.append(&header, fs::File::open(&input.path)?)?;
            }
        }
        let _ = builder.into_inner()?;
        Ok(path_transformer)
    }
}

#[cfg(feature = "dist-client")]
#[allow(unused)]
struct GoToolchainPackager {
    executable: PathBuf,
}

#[cfg(feature = "dist-client")]
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl pkg::ToolchainPackager for GoToolchainPackager {
    fn write_pkg(self: Box<Self>, f: fs::File) -> Result<()> {
        info!("Packaging Go compiler {}", self.executable.display());
        // The compiler is one executable and reads nothing else from GOROOT.
        let mut package_builder = pkg::ToolchainPackageBuilder::new();
        package_builder.add_common()?;
        package_builder.add_executable_and_deps(self.executable)?;
        package_builder.into_compressed_tar(f)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mock_command::*;
    use crate::test::utils::*;
    use std::sync::Mutex;

    fn parse(args: &[&str]) -> CompilerArguments<ParsedArguments> {
        parse_arguments(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    fn parsed(args: &[&str]) -> ParsedArguments {
        match parse(args) {
            CompilerArguments::Ok(parsed) => parsed,
            other => panic!("expected a cacheable compile, got {other:?}"),
        }
    }

    fn cannot_cache(args: &[&str]) -> (&'static str, Option<String>) {
        match parse(args) {
            CompilerArguments::CannotCache(why, extra) => (why, extra),
            other => panic!("expected an uncacheable compile, got {other:?}"),
        }
    }

    // What `go build` runs for a package, with the work directory spelled out.
    const GO_BUILD: &[&str] = &[
        "-o",
        "/work/b001/_pkg_.a",
        "-trimpath",
        "/work/b001=>",
        "-p",
        "example.com/hello",
        "-lang=go1.24",
        "-complete",
        "-buildid",
        "abc/abc",
        "-goversion",
        "go1.24.7",
        "-c=4",
        "-nolocalimports",
        "-importcfg",
        "/work/b001/importcfg",
        "-pack",
        "./a.go",
        "./b.go",
    ];

    #[test]
    fn parse_go_build_command_line() {
        let args = parsed(GO_BUILD);
        assert_eq!(args.output, Path::new("/work/b001/_pkg_.a"));
        assert_eq!(
            args.importcfg.as_deref(),
            Some(Path::new("/work/b001/importcfg"))
        );
        assert_eq!(args.trimpath, "/work/b001=>");
        assert_eq!(args.package, "example.com/hello");
        assert_eq!(args.sources, [Path::new("./a.go"), Path::new("./b.go")]);
        assert_eq!(
            args.hashed_flags,
            [
                "lang=go1.24",
                "complete",
                "buildid=abc/abc",
                "goversion=go1.24.7",
                "nolocalimports",
                "pack"
            ]
        );
        assert_eq!(args.arguments.len(), GO_BUILD.len());
        assert_eq!(
            (args.asmhdr, args.linkobj, args.embedcfg, args.symabis),
            (None, None, None, None)
        );
    }

    #[test]
    fn parse_flag_spellings() {
        let args = parsed(&[
            "--o=out.a",
            "-asmhdr=go_asm.h",
            "--symabis",
            "symabis",
            "-embedcfg",
            "embedcfg",
            "-linkobj=out.o",
            "-N",
            "-l=4",
            "--",
            "-odd.go",
        ]);
        assert_eq!(args.output, Path::new("out.a"));
        assert_eq!(args.asmhdr.as_deref(), Some(Path::new("go_asm.h")));
        assert_eq!(args.symabis.as_deref(), Some(Path::new("symabis")));
        assert_eq!(args.embedcfg.as_deref(), Some(Path::new("embedcfg")));
        assert_eq!(args.linkobj.as_deref(), Some(Path::new("out.o")));
        assert_eq!(args.hashed_flags, ["N", "l=4"]);
        assert_eq!(args.sources, [Path::new("-odd.go")]);
    }

    #[test]
    fn parse_version_query_is_not_a_compile() {
        assert!(matches!(
            parse(&["-V=full"]),
            CompilerArguments::NotCompilation
        ));
    }

    #[test]
    fn parse_rejects_what_it_does_not_track() {
        for flag in [
            "-S",
            "-m",
            "-d=ssa/check_bce/debug",
            "-json=0,dir",
            "-I=dir",
            "-linkshared",
        ] {
            let (why, extra) = cannot_cache(&["-o", "out.a", flag, "a.go"]);
            assert_eq!(why, "compiler flag", "{flag}");
            assert_eq!(extra.as_deref(), flag.split('=').next(), "{flag}");
        }
        for flag in ["-pgoprofile", "-coveragecfg", "-cpuprofile", "-memprofile"] {
            assert_eq!(
                cannot_cache(&["-o", "out.a", flag, "file", "a.go"]).0,
                "compiler flag"
            );
        }
        assert_eq!(
            cannot_cache(&["-o", "out.a", "a.go", "b.s"]).0,
            "source file"
        );
        assert_eq!(cannot_cache(&["-p", "x", "a.go"]).0, "no output file");
        assert_eq!(cannot_cache(&["-o", "out.a"]).0, "no source file");
        assert_eq!(cannot_cache(&["a.go", "-o"]).0, "source file");
        assert_eq!(cannot_cache(&["-o"]).0, "flag without a value");
    }

    fn tool(name: &str) -> Go {
        Go {
            executable: PathBuf::from("/go/pkg/tool/linux_amd64").join(name),
            executable_digest: "digest".to_owned(),
            tool: name.to_owned(),
            version: format!("{name} version go1.24.7"),
        }
    }

    fn compiler_parse(
        go: &Go,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> CompilerArguments<Box<dyn CompilerHasher<Arc<Mutex<MockCommandCreator>>>>> {
        let env = env
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect::<Vec<_>>();
        Compiler::<Arc<Mutex<MockCommandCreator>>>::parse_arguments(
            go,
            &args.iter().map(OsString::from).collect::<Vec<_>>(),
            Path::new("/src"),
            &env,
        )
    }

    #[test]
    fn only_the_compiler_is_cached() {
        assert!(matches!(
            compiler_parse(&tool("compile"), GO_BUILD, &[]),
            CompilerArguments::Ok(_)
        ));
        for name in ["asm", "link", "cgo", "vet"] {
            assert!(
                matches!(
                    compiler_parse(&tool(name), GO_BUILD, &[]),
                    CompilerArguments::NotCompilation
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn debug_dumps_are_not_cached() {
        assert!(matches!(
            compiler_parse(&tool("compile"), GO_BUILD, &[("GOSSAFUNC", "main")]),
            CompilerArguments::CannotCache("environment variable", Some(name)) if name == "GOSSAFUNC"
        ));
    }

    #[test]
    fn results_are_not_stored_twice() {
        let env = |value: &str| vec![(OsString::from("GOCACHEPROG"), OsString::from(value))];
        assert_eq!(cacheable(&[]), Cacheable::Yes);
        assert_eq!(cacheable(&env("")), Cacheable::Yes);
        assert_eq!(cacheable(&env("sccache --go-cacheprog")), Cacheable::No);
    }

    #[test]
    fn go_tool_detection() {
        let env = |name: &str| vec![(OsString::from(name), OsString::from("x"))];
        assert!(is_go_tool_like(
            Path::new("/usr/lib/go/pkg/tool/linux_amd64/compile"),
            &[]
        ));
        assert!(!is_go_tool_like(Path::new("/usr/bin/compile"), &[]));
        assert!(!is_go_tool_like(
            Path::new("/usr/lib/go/pkg/tool/compile"),
            &[]
        ));
        assert!(!is_go_tool_like(Path::new("compile"), &[]));
        assert!(is_go_tool_like(
            Path::new("/usr/bin/gcc"),
            &env("TOOLEXEC_IMPORTPATH")
        ));
        assert!(!is_go_tool_like(Path::new("/usr/bin/gcc"), &env("GOROOT")));
    }

    #[test]
    fn version_probe_identifies_go_tools() {
        let f = TestFixture::new();
        let runtime = single_threaded_runtime();
        let pool = runtime.handle();
        let creator = new_creator();
        let compile = f.mk_bin("compile").unwrap();

        next_command(
            &creator,
            Ok(MockChild::new(
                exit_status(0),
                "compile version go1.24.7\n",
                "",
            )),
        );
        let go = runtime
            .block_on(Go::new(creator.clone(), compile.clone(), &[], pool))
            .unwrap();
        assert_eq!(
            (go.tool.as_str(), go.version.as_str()),
            ("compile", "compile version go1.24.7")
        );

        // A wrapped C compiler does not answer like that.
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "gcc (GCC) 14.2.0\n", "")),
        );
        assert!(
            runtime
                .block_on(Go::new(creator.clone(), compile.clone(), &[], pool))
                .is_err()
        );
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(1), "", "unrecognized option")),
        );
        assert!(
            runtime
                .block_on(Go::new(creator, compile, &[], pool))
                .is_err()
        );
    }

    #[test]
    fn rewrites_match_the_compiler() {
        // Cases from cmd/internal/objabi.
        assert_eq!(apply_rewrites("/d/f.go", "/d=>"), Some("f.go".to_owned()));
        assert_eq!(apply_rewrites("/d", "/d=>x"), Some("x".to_owned()));
        assert_eq!(
            apply_rewrites("/d/f.go", "/d=>x"),
            Some("x/f.go".to_owned())
        );
        assert_eq!(apply_rewrites("/d/f.go", "/d"), Some("f.go".to_owned()));
        assert_eq!(
            apply_rewrites("/d/f.go", "/e=>x;/d=>y"),
            Some("y/f.go".to_owned())
        );
        assert_eq!(
            apply_rewrites("/d/f.go", "/d=>x;/d=>y"),
            Some("x/f.go".to_owned())
        );
        assert_eq!(apply_rewrites("/dd/f.go", "/d=>x"), None);
        assert_eq!(
            apply_rewrites("/D/f.go", "/d=>x"),
            Some("x/f.go".to_owned())
        );
        assert_eq!(apply_rewrites("/d/f.go", ""), None);
        assert_eq!(
            apply_rewrites("/a=>b/f.go", "/a=>b=>x"),
            Some("x/f.go".to_owned())
        );

        assert_eq!(
            recorded_path(Path::new("/go/src/fmt/print.go"), "/w=>", Some("/go")),
            "$GOROOT/src/fmt/print.go"
        );
        assert_eq!(
            recorded_path(Path::new("/go/src/fmt/print.go"), "/go=>go", Some("/go")),
            "go/src/fmt/print.go"
        );
        assert_eq!(
            recorded_path(Path::new("/gopher/x.go"), "/w=>", Some("/go")),
            "/gopher/x.go"
        );
        assert_eq!(
            recorded_path(Path::new("/go/src/x.go"), "/w=>", None),
            "/go/src/x.go"
        );
    }

    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut data = ARCHIVE_MAGIC.to_vec();
        for (name, contents) in members {
            data.extend(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    0,
                    0,
                    0,
                    644,
                    contents.len()
                )
                .bytes(),
            );
            data.extend(*contents);
            if contents.len() % 2 == 1 {
                data.push(b'\n');
            }
        }
        data
    }

    #[test]
    fn export_data_is_the_first_archive_member() {
        let f = TestFixture::new();
        let read = |name: &str, data: &[u8]| {
            let path = f.tempdir.path().join(name);
            fs::write(&path, data).unwrap();
            read_export_data(&path).unwrap()
        };

        let export_only = archive(&[("__.PKGDEF", b"export data")]);
        let full = archive(&[("__.PKGDEF", b"export data"), ("_go_.o", b"object code")]);
        assert_eq!(export_only.len() % 2, 0);
        assert_eq!(read("even.a", &full), export_only);
        assert_eq!(read("alone.a", &export_only), export_only);

        let odd = archive(&[("__.PKGDEF", b"export data!"), ("_go_.o", b"object code")]);
        assert_eq!(
            read("odd.a", &odd),
            archive(&[("__.PKGDEF", b"export data!")])
        );

        // Not what the compiler writes: nothing may be dropped.
        let other = archive(&[("_go_.o", b"object code"), ("__.PKGDEF", b"export data")]);
        assert_eq!(read("other.a", &other), other);
        assert_eq!(read("short", b"!<arch>\nshort"), b"!<arch>\nshort");
        assert_eq!(
            read(
                "text",
                b"plain text that is long enough to hold an archive header, and then some"
            ),
            b"plain text that is long enough to hold an archive header, and then some"
        );
        assert_eq!(read("empty", b""), b"");
    }

    /// A package in `src`, built in `work/b001` against a dependency.
    struct Package {
        f: TestFixture,
        env: Vec<(OsString, OsString)>,
    }

    impl Package {
        fn new() -> Self {
            let f = TestFixture::new();
            for dir in ["src", "work/b001", "work/b002", "elsewhere"] {
                fs::create_dir_all(f.tempdir.path().join(dir)).unwrap();
            }
            let package = Package {
                f,
                env: vec![
                    ("GOARCH".into(), "amd64".into()),
                    ("PATH".into(), "/bin".into()),
                ],
            };
            package.write("src/a.go", b"package hello\n");
            package.write("src/logo.png", b"png");
            package.write(
                "work/b002/_pkg_.a",
                &archive(&[("__.PKGDEF", b"exports"), ("_go_.o", b"code")]),
            );
            package.objdir("work/b001");
            package
        }

        fn path(&self, name: &str) -> PathBuf {
            self.f.tempdir.path().join(name)
        }

        fn write(&self, name: &str, contents: &[u8]) {
            fs::write(self.path(name), contents).unwrap();
        }

        /// Write what the go command generates into the work directory.
        fn objdir(&self, objdir: &str) {
            fs::create_dir_all(self.path(objdir)).unwrap();
            let importcfg = format!(
                "# import config\nimportmap old=new\npackagefile fmt={}\n",
                self.path("work/b002/_pkg_.a").display()
            );
            self.write(&format!("{objdir}/importcfg"), importcfg.as_bytes());
            let embedcfg = serde_json::json!({
                "Patterns": {"logo.png": ["logo.png"]},
                "Files": {"logo.png": self.path("src/logo.png")},
            });
            self.write(
                &format!("{objdir}/embedcfg"),
                embedcfg.to_string().as_bytes(),
            );
            self.write(&format!("{objdir}/symabis"), b"symabis");
        }

        fn args(&self, objdir: &str, extra: &[&str]) -> ParsedArguments {
            let objdir = self.path(objdir);
            let in_objdir = |name: &str| objdir.join(name).to_str().unwrap().to_owned();
            let mut args = vec![
                "-o".to_owned(),
                in_objdir("_pkg_.a"),
                "-trimpath".to_owned(),
                format!("{}=>", objdir.display()),
                "-p=example.com/hello".to_owned(),
                "-buildid=abc/abc".to_owned(),
                "-c=4".to_owned(),
                "-nolocalimports".to_owned(),
                format!("-importcfg={}", in_objdir("importcfg")),
                format!("-embedcfg={}", in_objdir("embedcfg")),
                format!("-symabis={}", in_objdir("symabis")),
                "-pack".to_owned(),
            ];
            args.extend(extra.iter().map(|arg| (*arg).to_owned()));
            args.push("./a.go".to_owned());
            parsed(&args.iter().map(String::as_str).collect::<Vec<_>>())
        }

        fn key_of(&self, args: &ParsedArguments, cwd: &str) -> String {
            hash_key(
                "digest",
                "compile version go1.24.7",
                args,
                &self.path(cwd),
                &self.env,
            )
            .unwrap()
            .0
        }

        fn key(&self) -> String {
            self.key_of(&self.args("work/b001", &[]), "src")
        }
    }

    #[test]
    fn hash_key_ignores_the_work_directory() {
        let p = Package::new();
        let key = p.key();
        assert_eq!(key, p.key());
        p.objdir("work/b077");
        assert_eq!(key, p.key_of(&p.args("work/b077", &[]), "src"));
    }

    #[test]
    fn hash_key_covers_the_source() {
        let p = Package::new();
        let key = p.key();
        p.write("src/a.go", b"package hello // changed\n");
        assert_ne!(key, p.key());
    }

    #[test]
    fn hash_key_covers_export_data_but_not_object_code() {
        let p = Package::new();
        let key = p.key();
        p.write(
            "work/b002/_pkg_.a",
            &archive(&[("__.PKGDEF", b"exports"), ("_go_.o", b"other code")]),
        );
        assert_eq!(key, p.key());
        p.write(
            "work/b002/_pkg_.a",
            &archive(&[("__.PKGDEF", b"exported"), ("_go_.o", b"code")]),
        );
        assert_ne!(key, p.key());
    }

    #[test]
    fn hash_key_covers_the_import_configuration() {
        let p = Package::new();
        let key = p.key();
        let importcfg = fs::read_to_string(p.path("work/b001/importcfg")).unwrap();
        p.write(
            "work/b001/importcfg",
            importcfg.replace("old=new", "old=newer").as_bytes(),
        );
        assert_ne!(key, p.key());
        p.write(
            "work/b001/importcfg",
            importcfg
                .replace("packagefile fmt=", "packagefile fmt2=")
                .as_bytes(),
        );
        assert_ne!(key, p.key());
    }

    #[test]
    fn hash_key_covers_embedded_files_and_symabis() {
        let p = Package::new();
        let key = p.key();
        p.write("src/logo.png", b"another png");
        assert_ne!(key, p.key());
        p.write("src/logo.png", b"png");
        assert_eq!(key, p.key());
        p.write("work/b001/symabis", b"other symabis");
        assert_ne!(key, p.key());
    }

    #[test]
    fn hash_key_covers_flags_but_not_the_thread_count() {
        let p = Package::new();
        let key = p.key();
        assert_ne!(key, p.key_of(&p.args("work/b001", &["-N"]), "src"));
        assert_ne!(
            key,
            p.key_of(&p.args("work/b001", &["-buildid=def/def"]), "src")
        );
        assert_ne!(
            key,
            p.key_of(&p.args("work/b001", &["-p=example.com/other"]), "src")
        );
        assert_eq!(key, p.key_of(&p.args("work/b001", &["-c=1"]), "src"));
    }

    #[test]
    fn hash_key_covers_the_compiler() {
        let p = Package::new();
        let args = p.args("work/b001", &[]);
        let key = |digest, version| {
            hash_key(digest, version, &args, &p.path("src"), &p.env)
                .unwrap()
                .0
        };
        assert_ne!(
            key("digest", "compile version go1.24.7"),
            key("other", "compile version go1.24.7")
        );
        assert_ne!(
            key("digest", "compile version go1.24.7"),
            key("digest", "compile version go1.24.8")
        );
    }

    #[test]
    fn hash_key_covers_the_compiler_environment_only() {
        let mut p = Package::new();
        let key = p.key();
        p.env.push(("GOCACHE".into(), "/cache".into()));
        p.env.push(("GOFLAGS".into(), "-toolexec=sccache".into()));
        p.env.push(("HOME".into(), "/home/user".into()));
        assert_eq!(key, p.key());
        p.env.push(("GOEXPERIMENT".into(), "rangefunc".into()));
        assert_ne!(key, p.key());
        p.env.pop();
        p.env[0].1 = "arm64".into();
        assert_ne!(key, p.key());
    }

    #[test]
    fn hash_key_follows_recorded_source_paths() {
        let p = Package::new();
        let key = p.key();
        p.write("elsewhere/a.go", b"package hello\n");
        p.write("elsewhere/logo.png", b"png");

        // Without -trimpath for the package directory, it ends up in the output.
        assert_ne!(key, p.key_of(&p.args("work/b001", &[]), "elsewhere"));

        let trim = |dir: &str| {
            let trimpath = format!(
                "-trimpath={}=>example.com/hello;{}=>",
                p.path(dir).display(),
                p.path("work/b001").display()
            );
            p.key_of(&p.args("work/b001", &[&trimpath]), dir)
        };
        assert_eq!(trim("src"), trim("elsewhere"));
        assert_ne!(trim("src"), key);
    }

    #[test]
    fn hash_key_covers_the_working_directory_when_the_compiler_uses_it() {
        let p = Package::new();
        let absolute = |mut args: ParsedArguments| {
            args.sources = vec![p.path("src/a.go")];
            args
        };
        let args = absolute(p.args("work/b001", &[]));
        assert_eq!(p.key_of(&args, "src"), p.key_of(&args, "elsewhere"));

        let mut local_imports = args.clone();
        local_imports
            .hashed_flags
            .retain(|flag| flag != "nolocalimports");
        assert_ne!(
            p.key_of(&local_imports, "src"),
            p.key_of(&local_imports, "elsewhere")
        );

        p.write("src/a.go", b"package hello\n//line relative.y:1\n");
        assert_ne!(p.key_of(&args, "src"), p.key_of(&args, "elsewhere"));
    }

    #[test]
    fn hash_key_covers_trimpath_when_sources_name_other_paths() {
        let p = Package::new();
        p.write("src/a.go", b"package hello\n//line /generated/from.y:1\n");
        let key = p.key();
        p.objdir("work/b077");
        assert_ne!(key, p.key_of(&p.args("work/b077", &[]), "src"));
        assert_ne!(
            key,
            p.key_of(&p.args("work/b001", &["-trimpath=/generated=>"]), "src")
        );

        p.write("src/a.go", b"package hello\n/*line :7*/ var x int\n");
        let key = p.key();
        assert_ne!(key, p.key_of(&p.args("work/b077", &[]), "src"));
    }

    #[test]
    fn inputs_are_everything_the_compiler_reads() {
        let p = Package::new();
        let (_, inputs) = hash_key(
            "digest",
            "version",
            &p.args("work/b001", &[]),
            &p.path("src"),
            &p.env,
        )
        .unwrap();
        // The source is named relative to the package directory.
        let mut expected = vec![(p.path("src").join("./a.go"), false)];
        for path in [
            "src/logo.png",
            "work/b001/embedcfg",
            "work/b001/importcfg",
            "work/b001/symabis",
        ] {
            expected.push((p.path(path), false));
        }
        expected.push((p.path("work/b002/_pkg_.a"), true));
        let mut expected = expected
            .into_iter()
            .map(|(path, export_data_only)| Input {
                path,
                export_data_only,
            })
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(inputs, expected);
    }

    #[test]
    fn missing_input_is_an_error() {
        let p = Package::new();
        fs::remove_file(p.path("work/b002/_pkg_.a")).unwrap();
        assert!(
            hash_key(
                "digest",
                "version",
                &p.args("work/b001", &[]),
                &p.path("src"),
                &p.env
            )
            .is_err()
        );
    }

    #[cfg(feature = "dist-client")]
    #[test]
    fn packaged_dependencies_carry_export_data_only() {
        use pkg::InputsPackager;

        let p = Package::new();
        let (_, inputs) = hash_key(
            "digest",
            "version",
            &p.args("work/b001", &[]),
            &p.path("src"),
            &p.env,
        )
        .unwrap();
        let mut tar = vec![];
        Box::new(GoInputsPackager {
            inputs,
            path_transformer: dist::PathTransformer::new(),
        })
        .write_inputs(&mut tar)
        .unwrap();

        let mut contents = BTreeMap::new();
        for entry in tar::Archive::new(tar.as_slice()).entries().unwrap() {
            let mut entry = entry.unwrap();
            let mut data = vec![];
            entry.read_to_end(&mut data).unwrap();
            contents.insert(Path::new("/").join(entry.path().unwrap()), data);
        }
        assert_eq!(contents.len(), 6);
        assert_eq!(
            contents[&p.path("work/b002/_pkg_.a")],
            archive(&[("__.PKGDEF", b"exports")])
        );
        assert_eq!(contents[&p.path("src/logo.png")], b"png");
        assert_eq!(
            contents[&p.path("work/b001/importcfg")],
            fs::read(p.path("work/b001/importcfg")).unwrap()
        );
    }
}
