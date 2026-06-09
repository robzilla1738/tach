//! The one Tach formatter.
//!
//! `tach fmt` renders the AST back to a single canonical spelling, so there is
//! never an argument about layout — the same program always formats the same
//! way. The output is deterministic and idempotent: formatting formatted source
//! is a no-op. Rendering is precedence-aware, so it only parenthesizes where
//! removing the parens would change the parse — never gratuitously.

use crate::ast::*;
use crate::parser::parse;

const STEP: usize = 2;

/// Why a file was left unformatted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The file has syntax errors — `tach check` will explain.
    ParseError,
    /// The file has comments, which the AST does not yet carry. Reformatting
    /// would delete them, so we refuse to touch it. (Comment-preserving
    /// formatting is a planned follow-up.)
    HasComments,
}

/// Format a source file to its canonical form, or report why it was skipped.
/// We never reformat a file we can't render losslessly: a parse error or any
/// comment leaves the file untouched.
pub fn format_file(path: &str, src: &str) -> Result<String, Skip> {
    if has_line_comment(src) {
        return Err(Skip::HasComments);
    }
    let (module, diags) = parse(path, src);
    if diags.iter().any(|d| d.is_error()) {
        return Err(Skip::ParseError);
    }
    Ok(format_module(&module))
}

/// Does the source contain a `//` line comment outside a string literal?
pub fn has_line_comment(src: &str) -> bool {
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
            return true;
        }
        i += 1;
    }
    false
}

/// Render a whole module to canonical source, items separated by a blank line.
pub fn format_module(m: &Module) -> String {
    let mut out = String::new();
    for (i, item) in m.items.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&fmt_item(item));
        out.push('\n');
    }
    out
}

fn pad(n: usize) -> String {
    " ".repeat(n)
}

fn fmt_item(item: &Item) -> String {
    match item {
        Item::Import(im) => format!("import {}", im.module),
        Item::Type(t) => fmt_type_decl(t),
        Item::Fn(f) => fmt_fn(f),
        Item::Test(t) => format!("test \"{}\" {}", t.name, fmt_block(&t.body, 0)),
        Item::Goal(g) => fmt_goal(g),
    }
}

/// Render a `goal` to its one canonical shape. Sections appear in a fixed order
/// (budget, allow, require) and empty sections are omitted, so the same goal
/// always formats the same way regardless of how it was authored.
fn fmt_goal(g: &GoalDecl) -> String {
    let head = match &g.success {
        Some(s) => format!("goal {} -> {} {{", g.name, s),
        None => format!("goal {} {{", g.name),
    };
    let mut s = head;
    s.push('\n');

    let b = &g.budget;
    if b.steps.is_some() || b.retries.is_some() || b.time.is_some() || b.cost.is_some() {
        s.push_str(&format!("{}budget {{\n", pad(STEP)));
        if let Some(v) = b.steps {
            s.push_str(&format!("{}steps: {}\n", pad(STEP * 2), v));
        }
        if let Some(v) = b.retries {
            s.push_str(&format!("{}retries: {}\n", pad(STEP * 2), v));
        }
        if let Some(t) = &b.time {
            s.push_str(&format!("{}time: {}\n", pad(STEP * 2), t));
        }
        if let Some(v) = b.cost {
            s.push_str(&format!("{}cost: {}\n", pad(STEP * 2), v));
        }
        s.push_str(&format!("{}}}\n", pad(STEP)));
    }

    let a = &g.allow;
    if !a.effects.is_empty()
        || !a.fs_read.is_empty()
        || !a.fs_write.is_empty()
        || !a.shell.is_empty()
        || !a.tools.is_empty()
    {
        s.push_str(&format!("{}allow {{\n", pad(STEP)));
        for e in &a.effects {
            s.push_str(&format!("{}effect {}\n", pad(STEP * 2), e.name));
        }
        if !a.fs_read.is_empty() {
            s.push_str(&format!(
                "{}fs.read {}\n",
                pad(STEP * 2),
                fmt_glob_list(&a.fs_read)
            ));
        }
        if !a.fs_write.is_empty() {
            s.push_str(&format!(
                "{}fs.write {}\n",
                pad(STEP * 2),
                fmt_glob_list(&a.fs_write)
            ));
        }
        if !a.shell.is_empty() {
            s.push_str(&format!(
                "{}shell.run {}\n",
                pad(STEP * 2),
                fmt_glob_list(&a.shell)
            ));
        }
        for t in &a.tools {
            s.push_str(&format!("{}{}\n", pad(STEP * 2), t));
        }
        s.push_str(&format!("{}}}\n", pad(STEP)));
    }

    let r = &g.require;
    if !r.conditions.is_empty() {
        s.push_str(&format!("{}require {{\n", pad(STEP)));
        for c in &r.conditions {
            s.push_str(&format!("{}{}\n", pad(STEP * 2), fmt_require_cond(c)));
        }
        s.push_str(&format!("{}}}\n", pad(STEP)));
    }

    if let Some(plan) = &g.plan {
        s.push_str(&format!("{}plan {{\n", pad(STEP)));
        s.push_str(&fmt_plan_stmts(&plan.stmts, STEP * 2));
        s.push_str(&format!("{}}}\n", pad(STEP)));
    }

    s.push('}');
    s
}

/// Render a plan body. Tool calls always lay their inputs out one field per line
/// (a `call` is the unit of work in a workflow — keeping each argument on its own
/// line keeps diffs and reviews legible); control flow nests by one step.
fn fmt_plan_stmts(stmts: &[PlanStmt], indent: usize) -> String {
    let mut s = String::new();
    for st in stmts {
        match st {
            PlanStmt::Let { name, value, .. } => {
                let rhs = match value {
                    PlanValue::Call(c) => fmt_plan_call(c, indent),
                    PlanValue::Expr(e) => fmt_expr(e, indent),
                };
                s.push_str(&format!("{}let {} = {}\n", pad(indent), name, rhs));
            }
            PlanStmt::Call { call, .. } => {
                s.push_str(&format!("{}{}\n", pad(indent), fmt_plan_call(call, indent)));
            }
            PlanStmt::Approve { summary, body, .. } => {
                s.push_str(&format!(
                    "{}approve \"{}\" {{\n",
                    pad(indent),
                    escape(summary)
                ));
                s.push_str(&fmt_plan_stmts(body, indent + STEP));
                s.push_str(&format!("{}}}\n", pad(indent)));
            }
            PlanStmt::If {
                cond, then, els, ..
            } => {
                s.push_str(&format!(
                    "{}if {} {{\n",
                    pad(indent),
                    fmt_expr(cond, indent)
                ));
                s.push_str(&fmt_plan_stmts(then, indent + STEP));
                if let Some(els) = els {
                    s.push_str(&format!("{}}} else {{\n", pad(indent)));
                    s.push_str(&fmt_plan_stmts(els, indent + STEP));
                }
                s.push_str(&format!("{}}}\n", pad(indent)));
            }
            PlanStmt::For {
                var, iter, body, ..
            } => {
                s.push_str(&format!(
                    "{}for {} in {} {{\n",
                    pad(indent),
                    var,
                    fmt_expr(iter, indent)
                ));
                s.push_str(&fmt_plan_stmts(body, indent + STEP));
                s.push_str(&format!("{}}}\n", pad(indent)));
            }
            PlanStmt::While { cond, body, .. } => {
                s.push_str(&format!(
                    "{}while {} {{\n",
                    pad(indent),
                    fmt_expr(cond, indent)
                ));
                s.push_str(&fmt_plan_stmts(body, indent + STEP));
                s.push_str(&format!("{}}}\n", pad(indent)));
            }
        }
    }
    s
}

/// `call <tool> { ... }` — an empty input renders inline, otherwise one field per line.
fn fmt_plan_call(c: &PlanCall, indent: usize) -> String {
    if c.input.is_empty() {
        return format!("call {} {{}}", c.tool);
    }
    let inner = indent + STEP;
    let mut s = format!("call {} {{\n", c.tool);
    for (k, e) in &c.input {
        s.push_str(&format!("{}{}: {}\n", pad(inner), k, fmt_expr(e, inner)));
    }
    s.push_str(&format!("{}}}", pad(indent)));
    s
}

/// Render a require condition: a bare predicate, or the parameterized
/// `command("…").passes` form when it carries an argument.
fn fmt_require_cond(c: &RequireCond) -> String {
    match (&c.arg, &c.pred) {
        (Some(arg), Some(pred)) => format!("{}(\"{}\").{}", c.name, escape(arg), pred),
        _ => c.name.clone(),
    }
}

/// A single glob renders bare; several render as a `[...]` list.
fn fmt_glob_list(globs: &[String]) -> String {
    if globs.len() == 1 {
        format!("\"{}\"", escape(&globs[0]))
    } else {
        let inner: Vec<String> = globs.iter().map(|g| format!("\"{}\"", escape(g))).collect();
        format!("[{}]", inner.join(", "))
    }
}

fn fmt_type_decl(d: &TypeDecl) -> String {
    match &d.ty {
        TypeExpr::Record { fields, .. } => {
            let mut s = format!("type {} = {{\n", d.name);
            for (n, ft) in fields {
                s.push_str(&format!("{}{}: {}\n", pad(STEP), n, fmt_type(ft)));
            }
            s.push('}');
            s
        }
        TypeExpr::Sum { variants, .. } => {
            let vs: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
            format!("type {} = {}", d.name, vs.join(" | "))
        }
        other => format!("type {} = {}", d.name, fmt_type(other)),
    }
}

/// Render a type in an inline position (param, return, field). Record types
/// render inline here; a top-level record `type` is handled multi-line above.
fn fmt_type(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Name { name, args, .. } => {
            if args.is_empty() {
                name.clone()
            } else {
                let a: Vec<String> = args.iter().map(fmt_type).collect();
                format!("{}<{}>", name, a.join(", "))
            }
        }
        TypeExpr::Record { fields, .. } => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(n, ft)| format!("{}: {}", n, fmt_type(ft)))
                .collect();
            format!("{{ {} }}", inner.join(", "))
        }
        TypeExpr::Sum { variants, .. } => variants
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn fmt_fn(f: &FnDecl) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, fmt_type(&p.ty)))
        .collect();
    let mut sig = format!("fn {}({})", f.name, params.join(", "));
    if let Some(ret) = &f.ret {
        sig.push_str(&format!(" -> {}", fmt_type(ret)));
    }
    if let Some(eff) = &f.effects {
        let names: Vec<&str> = eff.effects.iter().map(|e| e.name.as_str()).collect();
        sig.push_str(&format!(" effects [{}]", names.join(", ")));
    }
    format!("{} {}", sig, fmt_block(&f.body, 0))
}

/// Render a `{ ... }` block whose opening brace sits at column `indent`.
fn fmt_block(b: &Block, indent: usize) -> String {
    let inner = indent + STEP;
    let mut s = String::from("{\n");
    for stmt in &b.stmts {
        s.push_str(&pad(inner));
        s.push_str(&fmt_stmt(stmt, inner));
        s.push('\n');
    }
    s.push_str(&pad(indent));
    s.push('}');
    s
}

fn fmt_stmt(s: &Stmt, indent: usize) -> String {
    match s {
        Stmt::Let {
            name, ty, value, ..
        } => {
            let tyf = ty
                .as_ref()
                .map(|t| format!(": {}", fmt_type(t)))
                .unwrap_or_default();
            format!("let {}{} = {}", name, tyf, fmt_expr(value, indent))
        }
        Stmt::Return { value: Some(e), .. } => format!("return {}", fmt_expr(e, indent)),
        Stmt::Return { value: None, .. } => "return".into(),
        Stmt::Ensure { cond, els, .. } => {
            let mut out = format!("ensure {}", fmt_expr(cond, indent));
            if let Some(e) = els {
                out.push_str(&format!(" else {}", fmt_expr(e, indent)));
            }
            out
        }
        Stmt::If {
            cond, then, els, ..
        } => {
            let mut out = format!("if {} {}", fmt_expr(cond, indent), fmt_block(then, indent));
            if let Some(eb) = els {
                out.push_str(&format!(" else {}", fmt_block(eb, indent)));
            }
            out
        }
        Stmt::Expr(e) => fmt_expr(e, indent),
    }
}

// ----- expressions, precedence-aware -----

/// Binding power: higher binds tighter. Atoms and postfix forms sit above every
/// binary operator, so they never need wrapping as a child.
fn prec(e: &Expr) -> u8 {
    match e {
        Expr::Binary { op, .. } => match op {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
            BinOp::Add | BinOp::Sub => 4,
            BinOp::Mul | BinOp::Div => 5,
        },
        Expr::Unary { .. } => 6,
        _ => 7,
    }
}

fn fmt_expr(e: &Expr, indent: usize) -> String {
    match e {
        Expr::Int(n, _) => n.to_string(),
        Expr::Float(x, _) => x.to_string(),
        Expr::Str(s, _) => format!("\"{}\"", escape(s)),
        Expr::Bool(b, _) => b.to_string(),
        Expr::Ident(n, _) => n.clone(),
        Expr::Unary { op, expr, .. } => {
            let o = match op {
                UnOp::Not => "!",
                UnOp::Neg => "-",
            };
            // Wrap a binary operand so `-(a + b)` keeps its meaning.
            let inner = if matches!(**expr, Expr::Binary { .. }) {
                format!("({})", fmt_expr(expr, indent))
            } else {
                fmt_expr(expr, indent)
            };
            format!("{}{}", o, inner)
        }
        Expr::Binary { op, lhs, rhs, .. } => {
            let p = prec(e);
            // Left keeps its parse if its binding is >= ours; the right side of a
            // left-associative operator needs wrapping at equal precedence too.
            let l = wrap_child(lhs, p, false, indent);
            let r = wrap_child(rhs, p, true, indent);
            format!("{} {} {}", l, binop(*op), r)
        }
        Expr::Call { callee, args, .. } => {
            let a: Vec<String> = args.iter().map(|x| fmt_expr(x, indent)).collect();
            format!("{}({})", postfix_recv(callee, indent), a.join(", "))
        }
        Expr::Method {
            recv, name, args, ..
        } => {
            let a: Vec<String> = args.iter().map(|x| fmt_expr(x, indent)).collect();
            format!("{}.{}({})", postfix_recv(recv, indent), name, a.join(", "))
        }
        Expr::Field { recv, name, .. } => format!("{}.{}", postfix_recv(recv, indent), name),
        Expr::Try { expr, .. } => format!("{}?", postfix_recv(expr, indent)),
        Expr::Record { name, fields, .. } => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(n, e)| format!("{}: {}", n, fmt_expr(e, indent)))
                .collect();
            let prefix = name.clone().map(|n| format!("{} ", n)).unwrap_or_default();
            format!("{}{{ {} }}", prefix, inner.join(", "))
        }
        Expr::Ok(e, _) => format!("Ok({})", fmt_expr(e, indent)),
        Expr::Err(e, _) => format!("Err({})", fmt_expr(e, indent)),
        Expr::Match {
            scrutinee, arms, ..
        } => {
            let inner = indent + STEP;
            let mut s = format!("match {} {{\n", fmt_expr(scrutinee, indent));
            for arm in arms {
                s.push_str(&format!(
                    "{}{} => {}\n",
                    pad(inner),
                    pattern(&arm.pattern),
                    fmt_expr(&arm.body, inner)
                ));
            }
            s.push_str(&pad(indent));
            s.push('}');
            s
        }
    }
}

/// Render a binary child, parenthesizing only when dropping the parens would
/// reassociate the parse. `is_right` is true for the right operand.
fn wrap_child(child: &Expr, parent: u8, is_right: bool, indent: usize) -> String {
    let s = fmt_expr(child, indent);
    if matches!(child, Expr::Binary { .. }) {
        let cp = prec(child);
        let need = if is_right { cp <= parent } else { cp < parent };
        if need {
            return format!("({})", s);
        }
    }
    s
}

/// Render the receiver of a postfix form (`.field`, `.m()`, `(...)`, `?`),
/// wrapping any binary or unary so the postfix binds to the whole thing.
fn postfix_recv(e: &Expr, indent: usize) -> String {
    let s = fmt_expr(e, indent);
    if matches!(e, Expr::Binary { .. } | Expr::Unary { .. }) {
        format!("({})", s)
    } else {
        s
    }
}

fn pattern(p: &Pattern) -> String {
    match p {
        Pattern::Variant { name, .. } => name.clone(),
        Pattern::Wildcard { .. } => "_".into(),
    }
}

fn binop(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrips(src: &str) -> String {
        let once = format_file("t.tach", src).expect("formats");
        let twice = format_file("t.tach", &once).expect("re-formats");
        assert_eq!(once, twice, "formatter is not idempotent");
        // formatted source must still parse cleanly
        let (_, diags) = parse("t.tach", &once);
        assert!(
            diags.iter().all(|d| !d.is_error()),
            "formatted source has parse errors: {:?}",
            diags
        );
        once
    }

    #[test]
    fn formats_records_fns_and_tests() {
        let src = "type Session={token:String\nuser_id:Int}\nfn summary(s:Session)->Int{return s.user_id}\ntest \"x\"{ensure summary(Session{token:\"a\",user_id:1})==1}";
        let out = roundtrips(src);
        assert!(out.contains("type Session = {\n  token: String\n  user_id: Int\n}"));
        assert!(out.contains("fn summary(s: Session) -> Int {"));
    }

    #[test]
    fn formats_a_plan_goal_without_dropping_the_block() {
        // Regression: the formatter once rendered the goal head but silently dropped
        // the entire `plan { … }` body. A messy plan must round-trip to canonical form
        // with every statement intact.
        let src = "goal G -> Success {\n  budget { steps: 5 }\n  allow { fake.email.send }\n  plan {\n    for x in items {\n      approve \"ok\" { call fake.email.send { to: x.addr } }\n    }\n  }\n}\n";
        let out = roundtrips(src);
        assert!(out.contains("  plan {"), "plan block dropped: {out}");
        assert!(out.contains("    for x in items {"), "loop dropped: {out}");
        assert!(out.contains("approve \"ok\" {"), "gate dropped: {out}");
        assert!(
            out.contains("call fake.email.send {"),
            "call dropped: {out}"
        );
        assert!(out.contains("        to: x.addr"), "input dropped: {out}");
    }

    #[test]
    fn formats_sum_types_and_match() {
        let src = "type Parity = Even | Odd\nfn d(p: Parity) -> String {\n  return match p { Even => \"even\" Odd => \"odd\" }\n}\n";
        let out = roundtrips(src);
        assert!(out.contains("type Parity = Even | Odd"));
        assert!(out.contains("  return match p {\n    Even => \"even\"\n    Odd => \"odd\"\n  }"));
    }

    #[test]
    fn parenthesizes_only_where_meaning_would_change() {
        // (a + b) * c must keep its parens; a + b * c must not gain any.
        let out = roundtrips("fn f(a: Int, b: Int, c: Int) -> Int {\n  return (a + b) * c\n}\n");
        assert!(out.contains("(a + b) * c"), "got: {}", out);
        let out2 = roundtrips("fn f(a: Int, b: Int, c: Int) -> Int {\n  return a + b * c\n}\n");
        assert!(out2.contains("return a + b * c"), "got: {}", out2);
        // right-associative wrapping: a - (b - c) keeps parens.
        let out3 = roundtrips("fn f(a: Int, b: Int, c: Int) -> Int {\n  return a - (b - c)\n}\n");
        assert!(out3.contains("a - (b - c)"), "got: {}", out3);
    }

    #[test]
    fn already_formatted_is_a_fixed_point() {
        // The (comment-free) clean scaffold templates must already be canonical.
        for (name, src) in [
            ("main.tach", crate::project::CLEAN_MAIN),
            ("main_test.tach", crate::project::CLEAN_TEST),
            ("goal.tach", crate::project::DEMO_GOAL),
            // Plan goals must round-trip too — the formatter renders the whole
            // `plan { … }` block and never drops it.
            ("plan_demo.tach", crate::project::PLAN_DEMO_CHARGEBACKS),
        ] {
            let out = format_file(name, src).expect("formats");
            assert_eq!(out, src, "{} should be a formatting fixed point", name);
        }
    }

    #[test]
    fn corpus_is_canonically_formatted() {
        // Guards that every committed corpus file stays in canonical form — the
        // same guarantee `tach fmt --check corpus` gives, run hermetically in CI.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let cases = crate::project::load_suite(&dir).expect("load corpus");
        for (name, ws) in cases {
            for (path, text) in ws.files {
                match format_file(&path, &text) {
                    Ok(f) => assert_eq!(f, text, "corpus/{}/{} is not formatted", name, path),
                    Err(Skip::ParseError) => panic!("corpus/{}/{} does not parse", name, path),
                    Err(Skip::HasComments) => {}
                }
            }
        }
    }

    #[test]
    fn refuses_to_eat_comments() {
        // A file with comments is skipped, never reformatted — no data loss.
        let src = "// a note\nfn f() -> Int {\n  return 1\n}\n";
        assert_eq!(format_file("c.tach", src), Err(Skip::HasComments));
        // but `//` inside a string is not a comment.
        assert!(!has_line_comment(
            "fn f() -> String { return \"http://x\" }"
        ));
        assert!(has_line_comment("fn f() -> Int { return 1 } // tail"));
    }
}
