//! Errors and warnings reported by every phase of the compiler.

use crate::span::Span;

/// How serious a diagnostic is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The program is rejected.
    Error,
    /// The program is accepted, but something looks wrong.
    Warning,
}

/// A message about a span of source code, with optional extra labels and help.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    /// Error or warning.
    pub severity: Severity,
    /// The spec's error code, such as `N001`, when there is one.
    pub code: Option<&'static str>,
    /// What is wrong.
    pub message: String,
    /// Where it is wrong.
    pub span: Span,
    /// Other spans that explain the problem.
    pub labels: Vec<(Span, String)>,
    /// How to fix it.
    pub help: Option<String>,
}

impl Diagnostic {
    /// An error at `span`.
    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code: None,
            message: message.into(),
            span,
            labels: Vec::new(),
            help: None,
        }
    }

    /// Sets the error code.
    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    /// Adds a label on another span.
    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push((span, message.into()));
        self
    }

    /// Sets the help text.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}
