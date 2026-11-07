//! `noev lsp`: a language server for editors, over standard input and output.
//!
//! It runs the compiler's checks on every change and reports their diagnostics, and answers
//! hover (types and signatures), go to definition and document symbols from the checked
//! program. Documents are synced whole; programs are one file, and checking is fast.

use std::collections::HashMap;

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{DocumentSymbolRequest, GotoDefinition, HoverRequest, Request as _};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DocumentSymbol,
    DocumentSymbolResponse, GotoDefinitionResponse, Hover, HoverContents, HoverProviderCapability,
    InitializeParams, Location, MarkupContent, MarkupKind, NumberOrString, OneOf, Position,
    PositionEncodingKind, PublishDiagnosticsParams, Range, ServerCapabilities, SymbolKind,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use noev_sema::check::{Callee, Res};
use noev_sema::{Circuits, Ty, TyKind, Typed};

mod docs;
use noev_syntax::ast::{ExprId, ExprKind, FnId, ItemKind, TypeKind};
use noev_syntax::{Ast, Severity, Span};

/// Runs the server until the editor shuts it down.
pub fn serve() -> Result<(), String> {
    let (connection, io) = Connection::stdio();
    let (id, params) = connection.initialize_start().map_err(|e| e.to_string())?;
    let params: InitializeParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    // Positions in UTF-8 when the editor can, which is what the compiler counts in.
    let utf8 = params
        .capabilities
        .general
        .and_then(|g| g.position_encodings)
        .is_some_and(|e| e.contains(&PositionEncodingKind::UTF8));
    let capabilities = ServerCapabilities {
        position_encoding: Some(if utf8 {
            PositionEncodingKind::UTF8
        } else {
            PositionEncodingKind::UTF16
        }),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    };
    let result = serde_json::json!({
        "capabilities": capabilities,
        "serverInfo": { "name": "noev", "version": env!("CARGO_PKG_VERSION") },
    });
    connection
        .initialize_finish(id, result)
        .map_err(|e| e.to_string())?;

    let mut server = Server {
        connection: &connection,
        documents: HashMap::new(),
        utf8,
    };
    server.run()?;
    drop(connection);
    io.join().map_err(|e| e.to_string())
}

struct Server<'c> {
    connection: &'c Connection,
    /// Open documents by URI, with their latest analysis.
    documents: HashMap<String, Document>,
    /// Whether positions are counted in UTF-8 bytes rather than UTF-16 units.
    utf8: bool,
}

impl Server<'_> {
    fn run(&mut self) -> Result<(), String> {
        for message in &self.connection.receiver {
            match message {
                Message::Request(request) => {
                    if self
                        .connection
                        .handle_shutdown(&request)
                        .map_err(|e| e.to_string())?
                    {
                        return Ok(());
                    }
                    self.request(request);
                }
                Message::Notification(notification) => self.notification(notification),
                Message::Response(_) => {}
            }
        }
        Ok(())
    }

    fn notification(&mut self, notification: Notification) {
        match notification.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(
                    notification.params,
                ) else {
                    return;
                };
                let doc = params.text_document;
                self.update(doc.uri, doc.text);
            }
            DidChangeTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(
                    notification.params,
                ) else {
                    return;
                };
                // Full sync: the last change is the whole text.
                if let Some(change) = params.content_changes.into_iter().last() {
                    self.update(params.text_document.uri, change.text);
                }
            }
            DidCloseTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(
                    notification.params,
                ) else {
                    return;
                };
                self.documents.remove(params.text_document.uri.as_str());
                self.publish(params.text_document.uri, Vec::new());
            }
            _ => {}
        }
    }

    fn update(&mut self, uri: Uri, text: String) {
        let document = Document::new(text, self.utf8);
        let diagnostics = document.diagnostics(&uri);
        self.documents.insert(uri.as_str().to_string(), document);
        self.publish(uri, diagnostics);
    }

    fn publish(&self, uri: Uri, diagnostics: Vec<Diagnostic>) {
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: None,
        };
        self.send(Message::Notification(Notification::new(
            PublishDiagnostics::METHOD.to_string(),
            params,
        )));
    }

    fn request(&mut self, request: Request) {
        let id = request.id.clone();
        let result = match request.method.as_str() {
            HoverRequest::METHOD => self.position_request(request, |doc, offset, _| {
                serde_json::to_value(doc.hover(offset)).ok()
            }),
            GotoDefinition::METHOD => self.position_request(request, |doc, offset, uri| {
                let location = doc.definition(offset).map(|span| Location {
                    uri: uri.clone(),
                    range: doc.range(span),
                });
                serde_json::to_value(location.map(GotoDefinitionResponse::Scalar)).ok()
            }),
            DocumentSymbolRequest::METHOD => {
                let params =
                    serde_json::from_value::<lsp_types::DocumentSymbolParams>(request.params);
                params.ok().and_then(|p| {
                    let doc = self.documents.get(p.text_document.uri.as_str())?;
                    serde_json::to_value(DocumentSymbolResponse::Nested(doc.symbols())).ok()
                })
            }
            _ => {
                self.respond(Response::new_err(
                    id,
                    lsp_server::ErrorCode::MethodNotFound as i32,
                    format!("noev lsp does not handle {}", request.method),
                ));
                return;
            }
        };
        self.respond(Response::new_ok(
            id,
            result.unwrap_or(serde_json::Value::Null),
        ));
    }

    /// A request about a position in a document.
    fn position_request(
        &self,
        request: Request,
        answer: impl FnOnce(&Document, u32, &Uri) -> Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let params =
            serde_json::from_value::<lsp_types::TextDocumentPositionParams>(request.params).ok()?;
        let uri = params.text_document.uri;
        let doc = self.documents.get(uri.as_str())?;
        let offset = doc.offset(params.position)?;
        answer(doc, offset, &uri)
    }

    fn respond(&self, response: Response) {
        self.send(Message::Response(response));
    }

    fn send(&self, message: Message) {
        // The editor has gone if this fails; the receive loop ends the server.
        let _ = self.connection.sender.send(message);
    }
}

/// An open document and what the compiler found in it.
struct Document {
    text: String,
    /// The byte offset of the start of each line.
    lines: Vec<u32>,
    utf8: bool,
    parsed: noev_syntax::Parsed,
    tokens: Vec<noev_syntax::Token>,
    typed: Typed,
    found: Vec<noev_syntax::Diagnostic>,
    /// How each function runs, when the program passed the other checks.
    circuits: Option<Circuits>,
    /// Why each function is not reversible, when the program is well typed.
    reversible: Option<Vec<Option<(Span, String)>>>,
}

impl Document {
    /// Checks `text` as `noev check` does.
    fn new(text: String, utf8: bool) -> Self {
        let parsed = noev_syntax::parse(&text);
        let tokens = noev_syntax::lex(&text).tokens;
        let (typed, type_errors) = noev_sema::check(&parsed.ast);
        let mut found = parsed.diagnostics.clone();
        let mut circuits = None;
        let mut reversible = None;
        if found.is_empty() && type_errors.is_empty() {
            reversible = Some(noev_sema::reversibility(&parsed.ast, &typed));
            found.extend(noev_sema::check_ownership(&parsed.ast, &typed));
            if found.is_empty() {
                let (info, warnings) = noev_sema::check_circuits(&parsed.ast, &typed);
                found.extend(warnings);
                circuits = Some(info);
            }
        }
        found.extend(type_errors);
        let lines = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i as u32 + 1))
            .collect();
        Document {
            text,
            lines,
            utf8,
            parsed,
            tokens,
            typed,
            found,
            circuits,
            reversible,
        }
    }

    fn ast(&self) -> &Ast {
        &self.parsed.ast
    }

    // Positions

    fn position(&self, offset: u32) -> Position {
        let offset = offset.min(self.text.len() as u32);
        let line = self.lines.partition_point(|&start| start <= offset) - 1;
        let start = self.lines[line] as usize;
        let before = &self.text[start..offset as usize];
        let character = if self.utf8 {
            before.len()
        } else {
            before.encode_utf16().count()
        };
        Position::new(line as u32, character as u32)
    }

    fn range(&self, span: Span) -> Range {
        Range::new(self.position(span.start), self.position(span.end))
    }

    fn offset(&self, position: Position) -> Option<u32> {
        let start = *self.lines.get(position.line as usize)? as usize;
        let end = self
            .lines
            .get(position.line as usize + 1)
            .map_or(self.text.len(), |&e| e as usize);
        let line = &self.text[start..end];
        let mut units = 0;
        for (i, c) in line.char_indices() {
            if units >= position.character as usize {
                return Some((start + i) as u32);
            }
            units += if self.utf8 {
                c.len_utf8()
            } else {
                c.len_utf16()
            };
        }
        Some(end as u32)
    }

    // Diagnostics

    fn diagnostics(&self, uri: &Uri) -> Vec<Diagnostic> {
        self.found
            .iter()
            .map(|d| {
                let mut message = d.message.clone();
                if let Some(help) = &d.help {
                    message.push_str("\nhelp: ");
                    message.push_str(help);
                }
                let related = d
                    .labels
                    .iter()
                    .map(|(span, text)| DiagnosticRelatedInformation {
                        location: Location {
                            uri: uri.clone(),
                            range: self.range(*span),
                        },
                        message: text.clone(),
                    })
                    .collect::<Vec<_>>();
                Diagnostic {
                    range: self.range(d.span),
                    severity: Some(match d.severity {
                        Severity::Error => DiagnosticSeverity::ERROR,
                        Severity::Warning => DiagnosticSeverity::WARNING,
                    }),
                    code: d.code.map(|c| NumberOrString::String(c.to_string())),
                    source: Some("noev".into()),
                    message,
                    related_information: (!related.is_empty()).then_some(related),
                    ..Diagnostic::default()
                }
            })
            .collect()
    }

    // What is at a position

    /// The smallest expression containing `offset`.
    fn expr_at(&self, offset: u32) -> Option<ExprId> {
        let ast = self.ast();
        ast.all_exprs()
            .iter()
            .enumerate()
            .filter(|(_, e)| e.span.start <= offset && offset < e.span.end.max(e.span.start + 1))
            .min_by_key(|(_, e)| e.span.end - e.span.start)
            .map(|(i, _)| ExprId::from_index(i))
    }

    /// The call whose callee is `expr`, if any.
    fn call_of(&self, expr: ExprId) -> Option<ExprId> {
        let ast = self.ast();
        ast.all_exprs()
            .iter()
            .position(|e| matches!(e.kind, ExprKind::Call { callee, .. } if callee == expr))
            .map(ExprId::from_index)
    }

    /// The function `expr` calls, or names as the callee of a call.
    fn called(&self, expr: ExprId) -> Option<FnId> {
        let call = match self.ast().expr(expr).kind {
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => expr,
            _ => self.call_of(expr)?,
        };
        match self.typed.callees.get(call.index()).copied().flatten()? {
            Callee::Fn(f) => Some(f),
            _ => None,
        }
    }

    fn show(&self, ty: Ty) -> String {
        self.typed.types.display(ty, self.ast())
    }

    /// `fn name(params) -> ret`, from the checked signature.
    fn signature(&self, f: FnId) -> String {
        let ast = self.ast();
        let decl = ast.function(f);
        let Some(info) = self.typed.fns.get(f.index()) else {
            return String::new();
        };
        let mut params: Vec<String> = Vec::new();
        if let Some(receiver) = info.sig.receiver {
            params.push(format!("{} self", receiver.as_str()));
        }
        for (param, &ty) in ast.list(decl.params).iter().zip(&info.sig.params) {
            params.push(format!("{}: {}", ast.name(param.name), self.show(ty)));
        }
        let mut words = String::new();
        if decl.is_unsafe {
            words.push_str("unsafe ");
        }
        if decl.is_rev {
            words.push_str("rev ");
        }
        if decl.is_circuit {
            words.push_str("circuit ");
        }
        let owner = info
            .owner
            .map(|s| format!("{}::", ast.name(self.typed.types.structs[s.index()].name)))
            .unwrap_or_default();
        let ret = if info.sig.ret == Ty::UNIT {
            String::new()
        } else {
            format!(" -> {}", self.show(info.sig.ret))
        };
        format!(
            "{words}fn {owner}{}({}){ret}",
            ast.name(decl.name),
            params.join(", ")
        )
    }

    /// What hover shows: code (a signature, a declaration or a type), then an explanation.
    fn hover(&self, offset: u32) -> Option<Hover> {
        let (code, docs) = self.describe(offset)?;
        let mut value = format!("```noev\n{code}\n```");
        if !docs.is_empty() {
            value.push_str("\n\n");
            value.push_str(&docs);
        }
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        })
    }

    fn describe(&self, offset: u32) -> Option<(String, String)> {
        let ast = self.ast();
        if let Some(found) = self.describe_token(offset) {
            return Some(found);
        }
        if let Some(f) = self.function_named_at(offset) {
            return Some(self.describe_fn(f));
        }
        if let Some(found) = self.describe_declared_struct_or_field(offset) {
            return Some(found);
        }
        if let Some(local) = self.local_declared_at(offset) {
            return Some(self.describe_local(local));
        }
        if let Some((s, index)) = self.variant_pattern_at(offset) {
            return self.describe_variant(s, index);
        }
        if let Some(name) = self.type_named_at(offset)
            && let Some(found) = self.describe_struct(name)
        {
            return Some(found);
        }
        let expr = self.expr_at(offset)?;
        let kind = ast.expr(expr).kind;
        let names_callee = matches!(
            kind,
            ExprKind::Name(_) | ExprKind::Path(_) | ExprKind::MethodCall { .. }
        );
        if names_callee {
            if let Some(f) = self.called(expr) {
                return Some(self.describe_fn(f));
            }
            if let Some(callee) = self.callee(expr) {
                match callee {
                    Callee::Builtin(b) => return Some(docs::builtin(b)),
                    Callee::Convert(ty) => {
                        return Some(docs::builtin(noev_sema::prelude::Builtin::Convert(ty)));
                    }
                    Callee::Fn(_) => {}
                }
            }
        }
        match (kind, self.typed.res.get(expr.index()).copied()) {
            (ExprKind::Name(_), Some(Res::Local(local))) => {
                return Some(self.describe_local(local));
            }
            (_, Some(Res::Builtin(b))) => return Some(docs::builtin(b)),
            (_, Some(Res::Variant(s, index))) => return self.describe_variant(s, index),
            (ExprKind::Name(name), _) => {
                if let Some(found) = self.describe_struct(name) {
                    return Some(found);
                }
            }
            (ExprKind::Field { base, name }, _) => {
                if let Some(found) = self.describe_field_of(base, name) {
                    return Some(found);
                }
            }
            _ => {}
        }
        let ty = *self.typed.expr_types.get(expr.index())?;
        (ty != Ty::ERROR).then(|| (self.show(ty), String::new()))
    }

    /// Keywords, kets and bras.
    fn describe_token(&self, offset: u32) -> Option<(String, String)> {
        use noev_syntax::TokenKind;
        let token = self
            .tokens
            .iter()
            .find(|t| t.span.start <= offset && offset < t.span.end)?;
        let text = &self.text[token.span.start as usize..token.span.end as usize];
        match token.kind {
            TokenKind::Ket => Some(docs::ket(text)),
            TokenKind::Bra => Some(docs::bra(text)),
            TokenKind::Ident
            | TokenKind::Mut
            | TokenKind::Ref
            | TokenKind::Sample
            | TokenKind::Unsafe
            | TokenKind::Match
            | TokenKind::Enum
            | TokenKind::Assert => {
                // A contextual keyword used as a name is a name.
                let is_name = self
                    .ast()
                    .all_exprs()
                    .iter()
                    .any(|e| e.span == token.span && matches!(e.kind, ExprKind::Name(_)))
                    || self.typed.locals.iter().any(|l| l.span == token.span);
                if is_name {
                    return None;
                }
                let (code, doc) = docs::keyword(text)?;
                Some((code.to_string(), doc.to_string()))
            }
            _ => None,
        }
    }

    /// A function: its signature and doc comment, how it runs, and whether it is reversible.
    fn describe_fn(&self, f: FnId) -> (String, String) {
        let decl = self.ast().function(f);
        let mut notes = Vec::new();
        if let Some(doc) = self.doc_comment(decl.span) {
            notes.push(doc);
        }
        let mut facts = Vec::new();
        if let Some(info) = self.circuits.as_ref().and_then(|c| c.fns.get(f.index()))
            && info.quantum
        {
            let line = |span: Span| self.position(span.start).line + 1;
            let how = if let Some((span, what)) = &info.not_circuit {
                format!(
                    "not a circuit: a measurement result is {what} (line {})",
                    line(*span)
                )
            } else if let Some((span, why)) = &info.feedback_unsupported {
                format!(
                    "a circuit, but run shot by shot: {why} (line {})",
                    line(*span)
                )
            } else if let Some(span) = info.releases {
                format!(
                    "a circuit, but run shot by shot: it calls `release` (line {})",
                    line(span)
                )
            } else {
                "a circuit: sampled from one simulation when repeated".to_string()
            };
            facts.push(format!("**Runs as** {how}."));
        }
        if let Some(reversible) = &self.reversible
            && let Some(entry) = reversible.get(f.index())
        {
            let line = |span: Span| self.position(span.start).line + 1;
            facts.push(match entry {
                None if decl.is_rev => {
                    "**Reversible** (declared `rev`): usable under `adj` and in `ctrl`.".into()
                }
                None => "**Reversible** (inferred): usable under `adj` and in `ctrl`.".into(),
                Some((span, why)) => format!("**Not reversible**: {why} (line {}).", line(*span)),
            });
        }
        if !facts.is_empty() {
            notes.push(facts.join("  \n"));
        }
        (self.signature(f), notes.join("\n\n"))
    }

    fn describe_local(&self, local: noev_sema::check::LocalId) -> (String, String) {
        let ast = self.ast();
        let info = self.typed.local(local);
        let is_param = self.typed.fns.iter().any(|f| f.params.contains(&local));
        let name = ast.name(info.name);
        let ty = self.show(info.ty);
        let code = if is_param {
            format!("{name}: {ty}")
        } else if info.mutable {
            format!("let mut {name}: {ty}")
        } else {
            format!("let {name}: {ty}")
        };
        let linear = self.typed.types.linearity(info.ty) == noev_sema::ty::Kind::Linear;
        let doc = if linear {
            "Holds qubits, so it must be consumed exactly once: measured, discarded, moved \
             (`mv`) or returned."
                .to_string()
        } else {
            String::new()
        };
        (code, doc)
    }

    /// The struct called `name`: its fields and methods, and its doc comment.
    fn describe_struct(&self, name: noev_syntax::Symbol) -> Option<(String, String)> {
        let ast = self.ast();
        ast.all_items()
            .iter()
            .find_map(|item| match item.kind {
                ItemKind::Struct {
                    name: n,
                    fields,
                    methods,
                    ..
                } if n == name => {
                    let mut code = format!("struct {} {{\n", ast.name(name));
                    for field in ast.list(fields) {
                        let ty = self.field_type(name, field.name).unwrap_or_default();
                        code.push_str(&format!("    {}: {ty}\n", ast.name(field.name)));
                    }
                    for &m in ast.list(methods) {
                        code.push_str(&format!("    {}\n", self.signature(m)));
                    }
                    code.push('}');
                    Some((code, self.doc_comment(item.span).unwrap_or_default()))
                }
                ItemKind::Enum {
                    name: n,
                    variants,
                    methods,
                    ..
                } if n == name => {
                    let mut code = format!("enum {} {{\n", ast.name(name));
                    for variant in ast.list(variants) {
                        let text =
                            &self.text[variant.span.start as usize..variant.span.end as usize];
                        code.push_str(&format!("    {text}\n"));
                    }
                    for &m in ast.list(methods) {
                        code.push_str(&format!("    {}\n", self.signature(m)));
                    }
                    code.push('}');
                    Some((code, self.doc_comment(item.span).unwrap_or_default()))
                }
                _ => None,
            })
            .or_else(|| {
                // The standard library's enums.
                let def = self
                    .typed
                    .types
                    .structs
                    .iter()
                    .find(|d| d.name == name && d.span == Span::default())?;
                let variants: Vec<&str> = def
                    .variants
                    .iter()
                    .flatten()
                    .map(|v| ast.name(v.name))
                    .collect();
                let code = format!("enum {} {{ {} }}", ast.name(name), variants.join(", "));
                Some((code, docs::std_variant(ast.name(name), "")))
            })
    }

    /// A variant of the enum `s`: its declaration and the enum's doc comment.
    fn describe_variant(&self, s: noev_sema::ty::StructId, index: u32) -> Option<(String, String)> {
        let ast = self.ast();
        let def = &self.typed.types.structs[s.index()];
        let variant = def.variants.as_ref()?.get(index as usize)?;
        // The standard library's enums have no place in the file.
        if variant.span == Span::default() {
            let code = format!("{}::{}", ast.name(def.name), ast.name(variant.name));
            return Some((
                code,
                docs::std_variant(ast.name(def.name), ast.name(variant.name)),
            ));
        }
        let text = &self.text[variant.span.start as usize..variant.span.end as usize];
        let code = format!("{}::{text}", ast.name(def.name));
        let doc = self.doc_comment(variant.span).unwrap_or_default();
        Some((code, doc))
    }

    /// A struct or field where it is declared.
    fn describe_declared_struct_or_field(&self, offset: u32) -> Option<(String, String)> {
        let ast = self.ast();
        for item in ast.all_items() {
            let ItemKind::Struct {
                name,
                name_span,
                fields,
                ..
            } = item.kind
            else {
                continue;
            };
            if name_span.start <= offset && offset < name_span.end {
                return self.describe_struct(name);
            }
            for field in ast.list(fields) {
                if field.span.start <= offset && offset < field.span.end {
                    let ty = self.field_type(name, field.name).unwrap_or_default();
                    let doc = self.doc_comment(field.span).unwrap_or_default();
                    return Some((format!("{}: {ty}", ast.name(field.name)), doc));
                }
            }
        }
        None
    }

    /// The field `name` of `base`'s struct.
    fn describe_field_of(
        &self,
        base: ExprId,
        name: noev_syntax::Symbol,
    ) -> Option<(String, String)> {
        let ty = self
            .typed
            .types
            .deref(*self.typed.expr_types.get(base.index())?);
        let TyKind::Struct(s) = *self.typed.types.kind(ty) else {
            return None;
        };
        let def = &self.typed.types.structs[s.index()];
        let span = self.field_of(def.span, name)?;
        let ty = self.field_type(def.name, name).unwrap_or_default();
        let doc = self.doc_comment(span).unwrap_or_default();
        Some((format!("{}: {ty}", self.ast().name(name)), doc))
    }

    fn field_type(
        &self,
        strukt: noev_syntax::Symbol,
        field: noev_syntax::Symbol,
    ) -> Option<String> {
        let def = self.typed.types.structs.iter().find(|d| d.name == strukt)?;
        let &(_, ty) = def.fields.iter().find(|(n, _)| *n == field)?;
        Some(self.show(ty))
    }

    /// The `///` lines directly above the declaration starting at `span`.
    fn doc_comment(&self, span: Span) -> Option<String> {
        let line = self.position(span.start).line as usize;
        let mut docs = Vec::new();
        for l in (0..line).rev() {
            let start = self.lines[l] as usize;
            let end = self
                .lines
                .get(l + 1)
                .map_or(self.text.len(), |&e| e as usize);
            let text = self.text[start..end].trim();
            match text.strip_prefix("///") {
                Some(doc) => docs.push(doc.strip_prefix(' ').unwrap_or(doc).to_string()),
                None => break,
            }
        }
        docs.reverse();
        (!docs.is_empty()).then(|| docs.join("\n"))
    }

    /// What `expr` calls, or names as the callee of a call.
    fn callee(&self, expr: ExprId) -> Option<Callee> {
        let call = match self.ast().expr(expr).kind {
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => expr,
            _ => self.call_of(expr)?,
        };
        self.typed.callees.get(call.index()).copied().flatten()
    }

    /// The function whose name is at `offset`, in its declaration.
    fn function_named_at(&self, offset: u32) -> Option<FnId> {
        self.ast()
            .all_fns()
            .iter()
            .position(|f| f.name_span.start <= offset && offset < f.name_span.end)
            .map(FnId::from_index)
    }

    /// The variable declared at `offset` (a `let` pattern or a parameter).
    fn local_declared_at(&self, offset: u32) -> Option<noev_sema::check::LocalId> {
        self.typed
            .locals
            .iter()
            .position(|l| l.span.start <= offset && offset < l.span.end)
            .map(noev_sema::check::LocalId::from_index)
    }

    fn definition(&self, offset: u32) -> Option<Span> {
        let ast = self.ast();
        // A type name in an annotation.
        for ty in ast.all_types() {
            if ty.span.start <= offset
                && offset < ty.span.end
                && let TypeKind::Path(path) = ty.kind
                && let [name] = ast.list(path)
                && let Some(span) = self.struct_named(*name)
            {
                return Some(span);
            }
        }
        if let Some((s, index)) = self.variant_pattern_at(offset) {
            return self.variant_span(s, index);
        }
        let expr = self.expr_at(offset)?;
        if let Some(f) = self.called(expr) {
            return Some(ast.function(f).name_span);
        }
        match ast.expr(expr).kind {
            // A variable, or the name of a struct in a struct literal.
            ExprKind::Path(_) => match self.typed.res.get(expr.index())? {
                Res::Variant(s, index) => self.variant_span(*s, *index),
                _ => None,
            },
            ExprKind::Name(name) => match self.typed.res.get(expr.index())? {
                Res::Local(local) => Some(self.typed.local(*local).span),
                _ => self.struct_named(name),
            },
            ExprKind::Field { base, name } => {
                let ty = self
                    .typed
                    .types
                    .deref(*self.typed.expr_types.get(base.index())?);
                let TyKind::Struct(s) = *self.typed.types.kind(ty) else {
                    return None;
                };
                let def = &self.typed.types.structs[s.index()];
                self.field_of(def.span, name)
            }
            _ => None,
        }
    }

    /// The struct or enum a type annotation at `offset` names, with `Self` meaning the type
    /// whose method it is in.
    fn type_named_at(&self, offset: u32) -> Option<noev_syntax::Symbol> {
        let ast = self.ast();
        let ty = ast
            .all_types()
            .iter()
            .filter(|t| t.span.start <= offset && offset < t.span.end)
            .min_by_key(|t| t.span.end - t.span.start)?;
        let TypeKind::Path(path) = ty.kind else {
            return None;
        };
        let &[name] = ast.list(path) else { return None };
        if ast.name(name) != "Self" {
            return Some(name);
        }
        let (index, _) = ast
            .all_fns()
            .iter()
            .enumerate()
            .find(|(_, f)| f.span.start <= offset && offset < f.span.end)?;
        let owner = self.typed.fns.get(index)?.owner?;
        Some(self.typed.types.structs[owner.index()].name)
    }

    /// Where variant `index` of the enum `s` is declared.
    fn variant_span(&self, s: noev_sema::ty::StructId, index: u32) -> Option<Span> {
        let def = &self.typed.types.structs[s.index()];
        let span = def.variants.as_ref()?.get(index as usize)?.span;
        (span != Span::default()).then_some(span)
    }

    /// The enum and variant a variant pattern at `offset` names.
    fn variant_pattern_at(&self, offset: u32) -> Option<(noev_sema::ty::StructId, u32)> {
        let ast = self.ast();
        ast.all_pats()
            .iter()
            .enumerate()
            .filter(|(_, p)| p.span.start <= offset && offset < p.span.end)
            .filter_map(|(i, p)| {
                let index = (*self.typed.pat_variants.get(i)?)?;
                let ty = self.typed.types.deref(self.typed.pat_types[i]);
                match *self.typed.types.kind(ty) {
                    TyKind::Struct(s) => Some((p.span.end - p.span.start, s, index)),
                    _ => None,
                }
            })
            .min_by_key(|&(len, ..)| len)
            .map(|(_, s, index)| (s, index))
    }

    /// Where the struct or enum called `name` is declared.
    fn struct_named(&self, name: noev_syntax::Symbol) -> Option<Span> {
        self.ast()
            .all_items()
            .iter()
            .find_map(|item| match item.kind {
                ItemKind::Struct {
                    name: n, name_span, ..
                }
                | ItemKind::Enum {
                    name: n, name_span, ..
                } if n == name => Some(name_span),
                _ => None,
            })
    }

    /// Where field `name` of the struct named at `struct_span` is declared.
    fn field_of(&self, struct_span: Span, name: noev_syntax::Symbol) -> Option<Span> {
        let ast = self.ast();
        ast.all_items().iter().find_map(|item| match item.kind {
            ItemKind::Struct {
                name_span, fields, ..
            } if name_span == struct_span => ast
                .list(fields)
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.span),
            _ => None,
        })
    }

    #[allow(deprecated)] // `DocumentSymbol::deprecated` must still be given.
    fn symbols(&self) -> Vec<DocumentSymbol> {
        let ast = self.ast();
        let symbol =
            |name: String, detail: Option<String>, kind, span: Span, select: Span, children| {
                DocumentSymbol {
                    name,
                    detail,
                    kind,
                    tags: None,
                    deprecated: None,
                    range: self.range(span),
                    selection_range: self.range(select),
                    children,
                }
            };
        let function = |f: FnId, kind| {
            let decl = ast.function(f);
            symbol(
                ast.name(decl.name).to_string(),
                Some(self.signature(f)),
                kind,
                decl.span,
                decl.name_span,
                None,
            )
        };
        ast.list(ast.root)
            .iter()
            .filter_map(|&item| {
                let item = ast.item(item);
                match item.kind {
                    ItemKind::Fn(f) => Some(function(f, SymbolKind::FUNCTION)),
                    ItemKind::Enum {
                        name,
                        name_span,
                        variants,
                        methods,
                    } => {
                        let mut children: Vec<DocumentSymbol> = ast
                            .list(variants)
                            .iter()
                            .map(|v| {
                                symbol(
                                    ast.name(v.name).to_string(),
                                    None,
                                    SymbolKind::ENUM_MEMBER,
                                    v.span,
                                    v.span,
                                    None,
                                )
                            })
                            .collect();
                        children.extend(
                            ast.list(methods)
                                .iter()
                                .map(|&m| function(m, SymbolKind::METHOD)),
                        );
                        Some(symbol(
                            ast.name(name).to_string(),
                            None,
                            SymbolKind::ENUM,
                            item.span,
                            name_span,
                            Some(children),
                        ))
                    }
                    ItemKind::Struct {
                        name,
                        name_span,
                        fields,
                        methods,
                    } => {
                        let mut children: Vec<DocumentSymbol> = ast
                            .list(fields)
                            .iter()
                            .map(|f| {
                                symbol(
                                    ast.name(f.name).to_string(),
                                    None,
                                    SymbolKind::FIELD,
                                    f.span,
                                    f.span,
                                    None,
                                )
                            })
                            .collect();
                        children.extend(
                            ast.list(methods)
                                .iter()
                                .map(|&m| function(m, SymbolKind::METHOD)),
                        );
                        Some(symbol(
                            ast.name(name).to_string(),
                            None,
                            SymbolKind::STRUCT,
                            item.span,
                            name_span,
                            Some(children),
                        ))
                    }
                    _ => None,
                }
            })
            .collect()
    }
}
