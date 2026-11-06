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
use noev_sema::{Ty, TyKind, Typed};
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
    typed: Typed,
    found: Vec<noev_syntax::Diagnostic>,
}

impl Document {
    /// Checks `text` as `noev check` does.
    fn new(text: String, utf8: bool) -> Self {
        let parsed = noev_syntax::parse(&text);
        let (typed, type_errors) = noev_sema::check(&parsed.ast);
        let mut found = parsed.diagnostics.clone();
        if found.is_empty() && type_errors.is_empty() {
            found.extend(noev_sema::check_ownership(&parsed.ast, &typed));
            if found.is_empty() {
                found.extend(noev_sema::check_circuits(&parsed.ast, &typed).1);
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
            typed,
            found,
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

    fn hover(&self, offset: u32) -> Option<Hover> {
        let ast = self.ast();
        let text = self
            .function_named_at(offset)
            .map(|f| self.signature(f))
            .or_else(|| {
                let local = self.local_declared_at(offset)?;
                let local = self.typed.local(local);
                Some(format!(
                    "let {}: {}",
                    ast.name(local.name),
                    self.show(local.ty)
                ))
            })
            .or_else(|| {
                let expr = self.expr_at(offset)?;
                if let Some(f) = self.called(expr)
                    && matches!(
                        ast.expr(expr).kind,
                        ExprKind::Name(_) | ExprKind::Path(_) | ExprKind::MethodCall { .. }
                    )
                {
                    return Some(self.signature(f));
                }
                if let ExprKind::Name(_) = ast.expr(expr).kind
                    && let Some(Res::Local(local)) = self.typed.res.get(expr.index())
                {
                    let local = self.typed.local(*local);
                    return Some(format!("{}: {}", ast.name(local.name), self.show(local.ty)));
                }
                let ty = *self.typed.expr_types.get(expr.index())?;
                (ty != Ty::ERROR).then(|| self.show(ty))
            })?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```noev\n{text}\n```"),
            }),
            range: None,
        })
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
        let expr = self.expr_at(offset)?;
        if let Some(f) = self.called(expr) {
            return Some(ast.function(f).name_span);
        }
        match ast.expr(expr).kind {
            // A variable, or the name of a struct in a struct literal.
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

    /// Where the struct called `name` is declared.
    fn struct_named(&self, name: noev_syntax::Symbol) -> Option<Span> {
        self.ast()
            .all_items()
            .iter()
            .find_map(|item| match item.kind {
                ItemKind::Struct {
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
