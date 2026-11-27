//! Every program in the test suites and examples compiles the same when its file's offsets do
//! not start at 0, as they will not for every file of a package but the first
//! (`spec/modules.md` §8): the same diagnostics, moved by the file's start, and the same IR.

use std::fs;
use std::path::{Path, PathBuf};

use noev_syntax::{Diagnostic, SourceMap, Span};

fn nv_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        if path.is_dir() {
            nv_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "nv") {
            out.push(path);
        }
    }
}

/// Every diagnostic, with its spans relative to the file's start, and the IR if the program
/// was accepted.
fn compile(text: &str, padding: usize) -> (Vec<Diagnostic>, Option<String>) {
    let mut sources = SourceMap::default();
    if padding > 0 {
        sources.add("padding.nv", " ".repeat(padding)).unwrap();
    }
    let file = sources.add("test.nv", text).unwrap();
    let start = sources.file(file).start;
    let parsed = noev_syntax::parse_at(text, start);
    let (typed, mut diagnostics) = noev_sema::check(&parsed.ast);
    diagnostics.extend(parsed.diagnostics.iter().cloned());
    let mut ir = None;
    if diagnostics.is_empty() {
        diagnostics.extend(noev_sema::check_ownership(&parsed.ast, &typed));
        if diagnostics.is_empty() {
            let (circuits, warnings) = noev_sema::check_circuits(&parsed.ast, &typed);
            diagnostics.extend(warnings);
            let program = noev_ir::lower(&parsed.ast, &typed, &circuits, &sources);
            ir = Some(program.to_string());
        }
    }
    let local = |span: Span| {
        assert!(
            sources.file(file).contains(span.start),
            "{span:?} is outside the file"
        );
        Span::new(span.start - start, span.end - start)
    };
    for d in &mut diagnostics {
        d.span = local(d.span);
        for (span, _) in &mut d.labels {
            *span = local(*span);
        }
    }
    (diagnostics, ir)
}

#[test]
fn programs_compile_the_same_at_any_offset() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["tests/ui", "tests/run", "examples"] {
        nv_files(&root.join(dir), &mut files);
    }
    files.sort();
    assert!(files.len() > 50, "found only {} programs", files.len());
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        let at_zero = compile(&text, 0);
        let moved = compile(&text, 70_000);
        assert_eq!(
            at_zero,
            moved,
            "{} compiles differently at an offset",
            path.display()
        );
    }
}
