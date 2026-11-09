//! Loading a package: its manifest, its modules and its dependencies (`spec/modules.md` §1,
//! §7).
//!
//! A `.nv` file on its own is a package with one module. A directory is a package if it has
//! a `noev.toml`: each `.nv` file under its `src/` is a module named by its path, rooted at
//! `src/main.nv` (a program) or `src/lib.nv` (a library). Dependencies are other package
//! directories, given by `path`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use noev_syntax::ast::{Ast, ModuleId, PackageId};
use noev_syntax::{Diagnostic, SourceMap, Span, TokenKind};
use toml::de::{DeTable, DeValue};

/// Every source file of a package and its dependencies, parsed into one syntax tree.
pub struct Loaded {
    /// The files: manifests and modules.
    pub sources: SourceMap,
    /// The syntax tree of every module.
    pub ast: Ast,
    /// Errors in manifests, module names and syntax.
    pub diagnostics: Vec<Diagnostic>,
}

/// Why nothing could be checked at all.
pub enum LoadError {
    /// A file or directory could not be read.
    Io(PathBuf, std::io::Error),
    /// The files are larger than 4 GiB together.
    TooLarge,
    /// The path is not a `.nv` file or a package.
    NotAPackage(PathBuf),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(path, error) => write!(f, "cannot read {}: {error}", path.display()),
            LoadError::TooLarge => write!(f, "the source files are larger than 4 GiB together"),
            LoadError::NotAPackage(path) => write!(
                f,
                "{} is not a package: it has no noev.toml",
                path.display()
            ),
        }
    }
}

/// Loads the `.nv` file or package directory at `path`.
pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    load_with(path, &HashMap::new())
}

/// Loads the `.nv` file or package directory at `path`, with the files in `open` read from
/// there instead of from disk: the language server's open documents.
pub fn load_with(path: &Path, open: &HashMap<PathBuf, String>) -> Result<Loaded, LoadError> {
    let mut loader = Loader {
        open,
        loaded: Loaded {
            sources: SourceMap::default(),
            ast: Ast::new(),
            diagnostics: Vec::new(),
        },
        packages: HashMap::new(),
        stack: Vec::new(),
        manifest: 0,
    };
    if path.is_dir() {
        loader.package(path, None)?;
    } else {
        let text = loader.read(path)?;
        let package = loader.loaded.ast.add_package("main");
        let root = loader.loaded.ast.package(package).root;
        loader.module(root, path, text)?;
    }
    loader.loaded.ast.collect_root();
    Ok(loader.loaded)
}

/// The package directory a `.nv` file is in, if it is under a package's `src/`.
pub fn package_of(file: &Path) -> Option<PathBuf> {
    let mut dir = file.parent()?;
    loop {
        if dir.file_name().is_some_and(|n| n == "src")
            && let Some(package) = dir.parent()
            && package.join("noev.toml").is_file()
        {
            return Some(package.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Whether `name` can name a module or package: an identifier that is not a keyword.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name != "_"
        && TokenKind::keyword(name.as_bytes()).is_none()
        && !matches!(name, "pkg" | "super" | "std" | "core")
}

/// A dependency as its manifest gives it, for messages pointing at it.
struct Requested {
    /// Its name in the manifest.
    name: String,
    /// Where the manifest gives it.
    span: Span,
}

struct Loader<'o> {
    /// Files read from here instead of from disk.
    open: &'o HashMap<PathBuf, String>,
    loaded: Loaded,
    /// The packages loaded so far, by their directory.
    packages: HashMap<PathBuf, PackageId>,
    /// The packages being loaded, outermost first, to catch a cycle.
    stack: Vec<PathBuf>,
    /// The first offset of the manifest of the package whose modules are being loaded, for
    /// errors about files.
    manifest: u32,
}

impl Loader<'_> {
    fn read(&self, path: &Path) -> Result<String, LoadError> {
        if let Some(text) = self.open.get(path) {
            return Ok(text.clone());
        }
        std::fs::read_to_string(path).map_err(|e| LoadError::Io(path.to_path_buf(), e))
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.loaded
            .diagnostics
            .push(Diagnostic::error(span, message));
    }

    /// Adds `text`, the file at `path`, to the source map, and returns its first offset.
    fn add_file(&mut self, path: &Path, text: String) -> Result<u32, LoadError> {
        let id = self
            .loaded
            .sources
            .add(path.display().to_string(), text)
            .map_err(|_| LoadError::TooLarge)?;
        Ok(self.loaded.sources.file(id).start)
    }

    /// Parses `text`, the file at `path`, as the items of `module`. Returns the file's first
    /// offset.
    fn module(&mut self, module: ModuleId, path: &Path, text: String) -> Result<u32, LoadError> {
        let id = self
            .loaded
            .sources
            .add(path.display().to_string(), text)
            .map_err(|_| LoadError::TooLarge)?;
        let Loaded {
            sources,
            ast,
            diagnostics,
        } = &mut self.loaded;
        let file = sources.file(id);
        diagnostics.extend(noev_syntax::parse_module(
            ast, module, &file.text, file.start,
        ));
        Ok(file.start)
    }

    /// Loads the package in `dir`, which `requested` asks for if it is a dependency.
    fn package(
        &mut self,
        dir: &Path,
        requested: Option<&Requested>,
    ) -> Result<Option<PackageId>, LoadError> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| LoadError::Io(dir.to_path_buf(), e))?;
        if self.stack.contains(&canonical) {
            if let Some(requested) = requested {
                let message = format!(
                    "the dependency `{}` depends back on this package: packages cannot \
                     depend on each other in a cycle",
                    requested.name
                );
                self.error(requested.span, message);
            }
            return Ok(None);
        }
        if let Some(&id) = self.packages.get(&canonical) {
            return Ok(Some(id));
        }
        let manifest_path = dir.join("noev.toml");
        if !manifest_path.is_file() {
            return match requested {
                Some(requested) => {
                    let message = format!(
                        "the dependency `{}` is not a package: {} has no noev.toml",
                        requested.name,
                        dir.display()
                    );
                    self.error(requested.span, message);
                    Ok(None)
                }
                None => Err(LoadError::NotAPackage(dir.to_path_buf())),
            };
        }
        let text = self.read(&manifest_path)?;
        let base = self.add_file(&manifest_path, text.clone())?;
        let at = |range: std::ops::Range<usize>| {
            Span::new(base + range.start as u32, base + range.end as u32)
        };
        let manifest = match DeTable::parse(&text) {
            Ok(table) => table.into_inner(),
            Err(error) => {
                let span = error.span().map_or(Span::new(base, base), at);
                self.error(span, format!("invalid noev.toml: {}", error.message()));
                return Ok(None);
            }
        };

        // `[package] name = "…"`.
        let package_table = manifest.iter().find(|(k, _)| k.get_ref() == "package");
        let name = match package_table.map(|(_, v)| v) {
            Some(value) => match value.get_ref() {
                DeValue::Table(table) => match table.iter().find(|(k, _)| k.get_ref() == "name") {
                    Some((_, name)) => match name.get_ref() {
                        DeValue::String(s) => {
                            if !valid_name(s) {
                                let message = format!(
                                    "`{s}` is not a valid package name: it must be a name \
                                         Noev code can write, such as `quantum_tools`"
                                );
                                self.error(at(name.span()), message);
                            }
                            Some(s.to_string())
                        }
                        _ => {
                            self.error(at(name.span()), "the package's name is a string");
                            None
                        }
                    },
                    None => {
                        self.error(at(value.span()), "`[package]` needs a `name`");
                        None
                    }
                },
                _ => {
                    self.error(at(value.span()), "`package` is a table: `[package]`");
                    None
                }
            },
            None => {
                self.error(Span::new(base, base), "noev.toml needs a `[package]` table");
                None
            }
        };
        let Some(name) = name else {
            return Ok(None);
        };

        // The root module: `src/main.nv` for a program, `src/lib.nv` for a library.
        let src = dir.join("src");
        let main = src.join("main.nv");
        let lib = src.join("lib.nv");
        let root_file = match (main.is_file(), lib.is_file()) {
            (true, true) => {
                let message = "a package has `src/main.nv` (a program) or `src/lib.nv` (a \
                               library), not both";
                self.error(Span::new(base, base), message);
                return Ok(None);
            }
            (true, false) if requested.is_some() => {
                let requested = requested.expect("checked");
                let message = format!(
                    "the dependency `{}` is a program (`src/main.nv`), not a library \
                     (`src/lib.nv`)",
                    requested.name
                );
                self.error(requested.span, message);
                return Ok(None);
            }
            (true, false) => main,
            (false, true) => lib,
            (false, false) => {
                let message = "a package needs `src/main.nv` (a program) or `src/lib.nv` (a \
                               library)";
                self.error(Span::new(base, base), message);
                return Ok(None);
            }
        };

        let id = self.loaded.ast.add_package(&name);
        self.packages.insert(canonical.clone(), id);
        self.stack.push(canonical);
        let root = self.loaded.ast.package(id).root;
        let text = self.read(&root_file)?;
        self.module(root, &root_file, text)?;
        self.manifest = base;
        self.modules(&src, root, true)?;

        // `[dependencies] name = { path = "…" }`.
        if let Some((_, deps)) = manifest.iter().find(|(k, _)| k.get_ref() == "dependencies") {
            match deps.get_ref() {
                DeValue::Table(deps) => {
                    for (dep_name, spec) in deps {
                        let requested = Requested {
                            name: dep_name.get_ref().to_string(),
                            span: at(dep_name.span()),
                        };
                        if !valid_name(&requested.name) {
                            let message =
                                format!("`{}` is not a valid package name", requested.name);
                            self.error(requested.span, message);
                            continue;
                        }
                        let path = match spec.get_ref() {
                            DeValue::Table(spec) => spec
                                .iter()
                                .find(|(k, _)| k.get_ref() == "path")
                                .and_then(|(_, v)| match v.get_ref() {
                                    DeValue::String(s) => Some(s.to_string()),
                                    _ => None,
                                }),
                            _ => None,
                        };
                        let Some(path) = path else {
                            let message = if matches!(spec.get_ref(), DeValue::String(_)) {
                                "packages from a registry are not supported yet; give the \
                                 dependency's directory, as in `{ path = \"../qlib\" }`"
                            } else {
                                "a dependency needs a `path` to its directory, as in \
                                 `{ path = \"../qlib\" }`"
                            };
                            self.error(at(spec.span()), message);
                            continue;
                        };
                        if let Some(dep) = self.package(&dir.join(path), Some(&requested))? {
                            self.loaded.ast.add_dependency(id, &requested.name, dep);
                        }
                    }
                }
                _ => self.error(
                    at(deps.span()),
                    "`dependencies` is a table: `[dependencies]`",
                ),
            }
        }
        self.stack.pop();
        Ok(Some(id))
    }

    /// Loads the modules in the directory `dir`, inside `parent`. In `src/` itself, `main.nv`
    /// and `lib.nv` are the root module, already loaded.
    fn modules(&mut self, dir: &Path, parent: ModuleId, is_src: bool) -> Result<(), LoadError> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| LoadError::Io(dir.to_path_buf(), e))?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<Result<_, _>>()
            .map_err(|e| LoadError::Io(dir.to_path_buf(), e))?;
        entries.sort();
        // `a.nv` and the directory `a/` are one module.
        let mut modules: HashMap<String, ModuleId> = HashMap::new();
        for path in entries {
            let is_file = path.extension().is_some_and(|e| e == "nv") && path.is_file();
            if !is_file && !path.is_dir() {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if is_src && is_file && (stem == "main" || stem == "lib") {
                continue;
            }
            let module = match modules.get(&stem) {
                Some(&m) => m,
                None => {
                    let m = self.loaded.ast.add_module(parent, &stem);
                    modules.insert(stem.clone(), m);
                    if !valid_name(&stem) {
                        let message = format!(
                            "{} is not a valid module name: a module is named as Noev code \
                             writes it, such as `ladder.nv` or `circuits/`",
                            path.display()
                        );
                        self.error(Span::new(self.manifest, self.manifest), message);
                    }
                    m
                }
            };
            if is_file {
                let text = self.read(&path)?;
                self.module(module, &path, text)?;
            } else {
                self.modules(&path, module, false)?;
            }
        }
        Ok(())
    }
}
