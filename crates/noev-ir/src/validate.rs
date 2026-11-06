//! Checks that a lowered program is well formed: every block, local and function it refers to
//! exists, and each function has as many `ctrl` pops as pushes. (A `ctrl` body containing a
//! loop pushes in one block and pops in a later one, so balance is checked per function.)

use crate::ir::*;

/// Every problem found in `program`, as readable messages.
pub fn validate(program: &Program) -> Vec<String> {
    let mut problems = Vec::new();
    for function in &program.functions {
        let mut report = |message: String| problems.push(format!("{}: {message}", function.name));
        let blocks = function.blocks.len();
        let locals = function.locals.len();
        if blocks == 0 {
            report("no blocks".into());
            continue;
        }
        let check_place = |place: &Place, report: &mut dyn FnMut(String)| {
            if place.local.index() >= locals {
                report(format!("unknown local _{}", place.local.0));
            }
            for proj in &place.proj {
                if let Proj::Index(Operand::Copy(p) | Operand::Move(p)) = proj
                    && p.local.index() >= locals
                {
                    report(format!("unknown index local _{}", p.local.0));
                }
            }
        };
        let check_operand = |op: &Operand, report: &mut dyn FnMut(String)| {
            if let Operand::Copy(p) | Operand::Move(p) = op {
                check_place(p, report);
            }
        };
        for (b, block) in function.blocks.iter().enumerate() {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Assign(place, value) => {
                        check_place(place, &mut report);
                        match value {
                            Rvalue::Use(op)
                            | Rvalue::Unary(_, op)
                            | Rvalue::Repeat(op, _)
                            | Rvalue::Convert(_, op)
                            | Rvalue::Pauli(_, op) => check_operand(op, &mut report),
                            Rvalue::Binary(_, a, b) => {
                                check_operand(a, &mut report);
                                check_operand(b, &mut report);
                            }
                            Rvalue::Aggregate(ops) | Rvalue::Math(_, ops) => {
                                ops.iter().for_each(|op| check_operand(op, &mut report))
                            }
                            Rvalue::Ref { place, .. } => check_place(place, &mut report),
                            Rvalue::Projector { index, bit, .. } => {
                                check_operand(index, &mut report);
                                check_operand(bit, &mut report);
                            }
                            Rvalue::FloatConst(_) | Rvalue::Identity => {}
                            Rvalue::Format(parts) => {
                                for part in parts {
                                    if let FormatPart::Value(op, _, _) = part {
                                        check_operand(op, &mut report);
                                    }
                                }
                            }
                        }
                    }
                    Stmt::Alloc(p) | Stmt::Reset(p) | Stmt::Release(p) => {
                        check_place(p, &mut report)
                    }
                    Stmt::Gate { qubits, params, .. } => {
                        qubits.iter().for_each(|q| check_place(q, &mut report));
                        params.iter().for_each(|p| check_operand(p, &mut report));
                    }
                    Stmt::Measure { dest, qubit } => {
                        check_place(dest, &mut report);
                        check_place(qubit, &mut report);
                    }
                    Stmt::CtrlPush(places) => {
                        places.iter().for_each(|p| check_place(p, &mut report))
                    }
                    Stmt::CtrlPop => {}
                    Stmt::Print { args, .. } => args
                        .iter()
                        .for_each(|(op, _)| check_operand(op, &mut report)),
                    Stmt::Expectation {
                        dest,
                        prep,
                        observable,
                    } => {
                        check_place(dest, &mut report);
                        check_operand(observable, &mut report);
                        if let Prep::Call(func, args) = prep {
                            if func.index() >= program.functions.len() {
                                report(format!("unknown function {}", func.0));
                            }
                            args.iter().for_each(|a| check_operand(a, &mut report));
                        }
                    }
                }
            }
            let targets: Vec<Block> = match &block.term {
                Terminator::Goto(t) => vec![*t],
                Terminator::Branch {
                    cond,
                    then,
                    otherwise,
                } => {
                    check_operand(cond, &mut report);
                    vec![*then, *otherwise]
                }
                Terminator::Call {
                    func,
                    args,
                    dest,
                    next,
                    ..
                } => {
                    if func.index() >= program.functions.len() {
                        report(format!("unknown function {}", func.0));
                    }
                    args.iter().for_each(|a| check_operand(a, &mut report));
                    check_place(dest, &mut report);
                    vec![*next]
                }
                Terminator::Assert { cond, next, .. } => {
                    check_operand(cond, &mut report);
                    vec![*next]
                }
                Terminator::Return | Terminator::Unreachable => vec![],
            };
            for target in targets {
                if target.index() >= blocks {
                    report(format!("bb{b} jumps to missing bb{}", target.0));
                }
            }
        }
        // Every path pushes and pops in matching pairs, so the totals must agree.
        let pushes: usize = function
            .blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .filter(|s| matches!(s, Stmt::CtrlPush(_)))
            .count();
        let pops: usize = function
            .blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .filter(|s| matches!(s, Stmt::CtrlPop))
            .count();
        if pushes != pops {
            report(format!("{pushes} `ctrl push` but {pops} `ctrl pop`"));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower;

    fn lower_source(source: &str) -> Program {
        let parsed = noev_syntax::parse(source);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let (typed, errors) = noev_sema::check(&parsed.ast);
        assert!(errors.is_empty(), "{errors:?}");
        let errors = noev_sema::check_ownership(&parsed.ast, &typed);
        assert!(errors.is_empty(), "{errors:?}");
        lower(&parsed.ast, &typed, source)
    }

    #[test]
    fn bell_pair() {
        let program = lower_source(
            "fn bell_pair() -> (Bit, Bit) {\n let a = |+>\n let b = |0>\n ctrl a { b.x() }\n (mv a.measure(), mv b.measure())\n}",
        );
        let expected = "\
fn bell_pair() -> (Bit, Bit) {
    let _1: Qubit  // a
    let _2: Qubit  // b
    let _3: Bit
    let _4: Bit
  bb0:
    _1 = alloc
    h _1
    _2 = alloc
    ctrl push _1
    x _2
    ctrl pop
    _3 = measure _1
    _4 = measure _2
    _0 = {_3, _4}
    return
}
";
        assert_eq!(program.to_string(), expected);
    }

    #[test]
    fn standard_library_lowers_to_its_definition() {
        let program = lower_source("fn f() {\n let q = |->\n mv q.discard()\n}");
        let text = program.to_string();
        assert!(
            text.contains("x _1\n    h _1\n    reset _1\n    release _1"),
            "{text}"
        );
    }

    #[test]
    fn adjoint_calls_and_gates() {
        let program = lower_source(
            "fn g(q: mut Qubit) { q.s() }\nfn f() {\n let q = |0>\n adj g(q)\n adj q.t()\n mv q.discard()\n}",
        );
        let text = program.to_string();
        assert!(text.contains("= adj g("), "{text}");
        assert!(text.contains("adj t _1"), "{text}");
    }

    #[test]
    fn ctrl_around_a_loop() {
        let program = lower_source(
            "fn f() -> [Bit; 3] {\n let r = |+00>\n ctrl r[0] {\n for i in 1..3 { r[i].x() }\n }\n mv r.measure()\n}",
        );
        assert!(validate(&program).is_empty(), "{program}");
    }

    #[test]
    fn ui_programs_lower_to_valid_ir() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/ui/pass");
        let mut count = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let source = std::fs::read_to_string(&path).unwrap();
            let program = lower_source(&source);
            let problems = validate(&program);
            assert!(problems.is_empty(), "{}: {problems:?}", path.display());
            count += 1;
        }
        assert!(count > 0);
    }
}
