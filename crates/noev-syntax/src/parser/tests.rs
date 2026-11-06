use crate::ast::*;
use crate::parser::parse;

/// Renders an expression as an S-expression, for checking structure and precedence.
fn sexp(ast: &Ast, source: &str, id: ExprId) -> String {
    let e = |id| sexp(ast, source, id);
    let list = |l: List<ExprId>| {
        ast.list(l)
            .iter()
            .map(|&id| sexp(ast, source, id))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let expr = ast.expr(id);
    match expr.kind {
        ExprKind::Error => "<error>".into(),
        ExprKind::Int(v) => v.to_string(),
        ExprKind::Float(v) => format!("{v:?}"),
        ExprKind::Bool(v) => v.to_string(),
        ExprKind::Ket => expr.span.text(source).into(),
        ExprKind::Name(s) => ast.name(s).into(),
        ExprKind::Path(p) => ast
            .list(p)
            .iter()
            .map(|&s| ast.name(s))
            .collect::<Vec<_>>()
            .join("::"),
        ExprKind::Unit => "()".into(),
        ExprKind::Tuple(l) => format!("(tuple {})", list(l)),
        ExprKind::Array(l) => format!("(array {})", list(l)),
        ExprKind::ArrayRepeat { value, count } => format!("(repeat {} {})", e(value), e(count)),
        ExprKind::StructLit { path, fields } => {
            let fields: Vec<String> = ast
                .list(fields)
                .iter()
                .map(|f| match f.value {
                    Some(v) => format!("{}: {}", ast.name(f.name), e(v)),
                    None => ast.name(f.name).into(),
                })
                .collect();
            format!("(struct {} {})", e(path), fields.join(" "))
        }
        ExprKind::Field { base, name } => format!("(. {} {})", e(base), ast.name(name)),
        ExprKind::TupleIndex { base, index } => format!("(. {} {index})", e(base)),
        ExprKind::Index { base, index } => format!("(index {} {})", e(base), e(index)),
        ExprKind::Call { callee, args } => format!("(call {} {})", e(callee), list(args)),
        ExprKind::MethodCall {
            receiver,
            method,
            args,
        } => format!(
            "(method {} {} {})",
            e(receiver),
            ast.name(method),
            list(args)
        ),
        ExprKind::Unary { op, operand } => format!("({op:?} {})", e(operand)),
        ExprKind::Binary { op, lhs, rhs } => format!("({} {} {})", op.as_str(), e(lhs), e(rhs)),
        ExprKind::Own { kind, place } => format!("({} {})", kind.as_str(), e(place)),
        ExprKind::Adj(call) => format!("(adj {})", e(call)),
        ExprKind::BraTest { state, operand } => {
            format!("(test {} {})", state.text(source), e(operand))
        }
        ExprKind::Expectation { prep, observable } => {
            format!("(expect {} {})", e(prep), e(observable))
        }
        ExprKind::Block(_) => "(block)".into(),
        ExprKind::If {
            cond, otherwise, ..
        } => match otherwise {
            Some(o) => format!("(if {} {})", e(cond), e(o)),
            None => format!("(if {})", e(cond)),
        },
        ExprKind::While { cond, .. } => format!("(while {})", e(cond)),
        ExprKind::For {
            var, start, end, ..
        } => {
            format!("(for {} {} {})", ast.name(var), e(start), e(end))
        }
        ExprKind::Ctrl { controls, .. } => format!("(ctrl {})", list(controls)),
        ExprKind::With { .. } => "(with)".into(),
        ExprKind::Unsafe(_) => "(unsafe)".into(),
    }
}

/// Parses `fn main() { <stmt> }` and renders the statement's expression.
fn expr(stmt: &str) -> String {
    let source = format!("fn main() {{\n{stmt}\n}}");
    let parsed = parse(&source);
    assert!(
        parsed.diagnostics.is_empty(),
        "{stmt}: {:?}",
        parsed.diagnostics
    );
    let ast = &parsed.ast;
    let ItemKind::Fn(f) = ast.item(ast.list(ast.root)[0]).kind else {
        panic!("expected a function")
    };
    let body = ast.block(ast.function(f).body.unwrap());
    let stmts = ast.list(body.stmts);
    assert_eq!(stmts.len(), 1, "{stmt}: expected one statement");
    match ast.stmt(stmts[0]).kind {
        StmtKind::Expr { expr, .. } => sexp(ast, &source, expr),
        StmtKind::Let { init, .. } => sexp(ast, &source, init),
        StmtKind::Assign { place, value, .. } => {
            format!(
                "(= {} {})",
                sexp(ast, &source, place),
                sexp(ast, &source, value)
            )
        }
        other => format!("{other:?}"),
    }
}

fn errors(source: &str) -> Vec<String> {
    parse(source)
        .diagnostics
        .into_iter()
        .map(|d| d.message)
        .collect()
}

#[test]
fn arithmetic_precedence() {
    assert_eq!(expr("a + b * c"), "(+ a (* b c))");
    assert_eq!(expr("a - b - c"), "(- (- a b) c)");
    assert_eq!(expr("-a * b"), "(* (Neg a) b)");
    assert_eq!(expr("a | b ^ c & d << 1"), "(| a (^ b (& c (<< d 1))))");
}

#[test]
fn logic_precedence() {
    assert_eq!(expr("a and b or c"), "(or (and a b) c)");
    assert_eq!(expr("not a == b"), "(Not (== a b))");
    assert_eq!(expr("a < b and not c"), "(and (< a b) (Not c))");
}

#[test]
fn postfix_chains() {
    assert_eq!(expr("q.h().s()"), "(method (method q h ) s )");
    assert_eq!(
        expr("e.state.ry(e.angle)"),
        "(method (. e state) ry (. e angle))"
    );
    assert_eq!(expr("r[i + 1].x()"), "(method (index r (+ i 1)) x )");
    assert_eq!(expr("p.0.1"), "(. (. p 0) 1)");
    assert_eq!(expr("Float::PI / Float(n)"), "(/ Float::PI (call Float n))");
}

#[test]
fn ownership_words() {
    assert_eq!(
        expr("mv e.state.measure()"),
        "(method (mv (. e state)) measure )"
    );
    assert_eq!(expr("bump(mut n)"), "(call bump (mut n))");
    assert_eq!(expr("peek(ref n)"), "(call peek (ref n))");
    assert_eq!(
        expr("(mv a.measure(), mv b.measure())"),
        "(tuple (method (mv a) measure ) (method (mv b) measure ))"
    );
    assert_eq!(expr("adj q.s()"), "(adj (method q s ))");
}

#[test]
fn contextual_keywords_as_names() {
    assert_eq!(expr("let adj = 3"), "3");
    assert_eq!(expr("adj + 1"), "(+ adj 1)");
    assert_eq!(expr("mv(x)"), "(call mv x)");
}

#[test]
fn literals_and_compounds() {
    assert_eq!(expr("|+0->"), "|+0->");
    assert_eq!(expr("0xff + 0b10 + 1_000"), "(+ (+ 255 2) 1000)");
    assert_eq!(expr("2.5e6"), "2500000.0");
    assert_eq!(expr("[|0>; 3]"), "(repeat |0> 3)");
    assert_eq!(expr("[1, 2]"), "(array 1 2)");
    assert_eq!(expr("()"), "()");
    assert_eq!(
        expr("Experiment { angle, state: |0> }"),
        "(struct Experiment angle state: |0>)"
    );
}

#[test]
fn control_flow() {
    assert_eq!(
        expr("if x { a } else if y { b } else { c }"),
        "(if x (if y (block)))"
    );
    assert_eq!(expr("for i in 0..n - 1 { }"), "(for i 0 (- n 1))");
    assert_eq!(expr("ctrl a, r[0] { b.x() }"), "(ctrl a (index r 0))");
    assert_eq!(expr("with { f(a) } do { g(b) }"), "(with)");
    assert_eq!(expr("unsafe { mv q.release() }"), "(unsafe)");
}

#[test]
fn struct_literal_not_allowed_in_conditions() {
    assert_eq!(expr("if x { y }"), "(if x)");
    assert_eq!(expr("while done { }"), "(while done)");
}

#[test]
fn bras() {
    assert_eq!(expr("<1| mv q"), "(test <1| (mv q))");
    assert_eq!(expr("<1| mv q and ok"), "(and (test <1| (mv q)) ok)");
    assert_eq!(expr("<+| Z(0) |+>"), "(expect |+> (call Z 0))");
    assert_eq!(expr("<+-| Z(0) |+->"), "(expect |+-> (call Z 0))");
    assert_eq!(
        expr("<ansatz(t)| -1.0 * Z(0) + X(1) |ansatz(t)>"),
        "(expect (call ansatz t) (+ (* (Neg 1.0) (call Z 0)) (call X 1)))"
    );
}

#[test]
fn expectation_must_repeat_its_preparation() {
    let errs = errors("fn f() { <a(1)| Z(0) |a(2)> }");
    assert_eq!(
        errs,
        ["both sides of an expectation value must be the same preparation"]
    );
    let errs = errors("fn f() { <+| Z(0) |0> }");
    assert_eq!(
        errs,
        ["both sides of an expectation value must be the same state"]
    );
}

#[test]
fn line_breaks_end_statements() {
    let source = "fn f() {\n let a = b\n -c\n let d = e >\n f\n let g = <p()| Z(0) |p()>\n h()\n}";
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let ast = &parsed.ast;
    let ItemKind::Fn(f) = ast.item(ast.list(ast.root)[0]).kind else {
        panic!()
    };
    let body = ast.block(ast.function(f).body.unwrap());
    assert_eq!(ast.list(body.stmts).len(), 5);
}

#[test]
fn items() {
    let source = "struct Pair {\n a: Qubit, b: Qubit\n fn swap(mut self) -> mut Self { mut self }\n}\nunsafe rev fn f(x: mut [Qubit; 4]) {}\ntrait T { type R\n fn m(mv self) -> Self::R }\nimpl T for Pair { type R = Int\n fn m(mv self) -> Int { 0 } }";
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let ast = &parsed.ast;
    let items = ast.list(ast.root);
    assert_eq!(items.len(), 4);
    let ItemKind::Struct {
        fields, methods, ..
    } = ast.item(items[0]).kind
    else {
        panic!()
    };
    assert_eq!((fields.len(), methods.len()), (2, 1));
    let method = ast.function(ast.list(methods)[0]);
    assert_eq!(method.receiver.map(|r| r.kind), Some(OwnKind::Mut));
    let ItemKind::Fn(f) = ast.item(items[1]).kind else {
        panic!()
    };
    let f = ast.function(f);
    assert!(f.is_unsafe && f.is_rev);
}

#[test]
fn helpful_errors() {
    assert_eq!(
        errors("fn f() { a && b }"),
        ["Noev writes logical and as `and`"]
    );
    assert_eq!(
        errors("fn f() { a || b }"),
        ["Noev writes logical or as `or`"]
    );
    assert_eq!(
        errors("fn f() { !a }"),
        ["Noev writes logical not as `not`"]
    );
    assert_eq!(
        errors("fn f() { a < b < c }"),
        ["comparisons cannot be chained"]
    );
    assert_eq!(
        errors("struct S { fn m(self) {} }"),
        ["say how the method takes `self`"]
    );
    assert_eq!(
        errors("extern fn f(x: Float) -> Float\nfn g() {}"),
        ["`extern` functions are not supported yet"]
    );
    assert_eq!(
        errors("fn g() { let extern = 1 }"),
        ["`extern` is a reserved keyword and cannot be used as a name"]
    );
}

#[test]
fn recovers_and_reports_every_error() {
    let errs = errors("fn f() {\n let = 1\n let b = )\n}\nfn g() { h( }");
    assert_eq!(errs.len(), 3, "{errs:?}");
}

#[test]
fn ui_tests_parse_cleanly() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/ui");
    let mut stack = vec![std::path::PathBuf::from(dir)];
    let mut count = 0;
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        } else if path.extension().is_some_and(|e| e == "nv") {
            let source = std::fs::read_to_string(&path).unwrap();
            let parsed = parse(&source);
            assert!(
                parsed.diagnostics.is_empty(),
                "{}: {:?}",
                path.display(),
                parsed.diagnostics
            );
            count += 1;
        }
    }
    assert!(count > 0);
}
