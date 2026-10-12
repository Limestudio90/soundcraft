//! EEL2 parser: precedence climbing over the token stream, producing [`Expr`] with variable names
//! already resolved to slots.

use crate::JsfxError;
use crate::ast::{BinOp, Builtin, Expr, UnOp};
use crate::lex::{Lexed, Tok, lex};
use std::collections::HashMap;

/// Reserved variable slots shared by the interpreter and the plugin wrapper.
pub(crate) const SPL0: usize = 0;
pub(crate) const MAX_CH: usize = 64;
pub(crate) const SLIDER1: usize = 64;
pub(crate) const MAX_SLIDERS: usize = 64;
pub(crate) const SRATE: usize = 128;
pub(crate) const NUM_CH: usize = 129;
pub(crate) const SAMPLESBLOCK: usize = 130;
pub(crate) const TEMPO: usize = 131;
pub(crate) const PLAY_STATE: usize = 132;
pub(crate) const PLAY_POSITION: usize = 133;
pub(crate) const BEAT_POSITION: usize = 134;
pub(crate) const TS_NUM: usize = 135;
pub(crate) const TS_DENOM: usize = 136;
pub(crate) const PDC_DELAY: usize = 137;
pub(crate) const PDC_BOT_CH: usize = 138;
pub(crate) const PDC_TOP_CH: usize = 139;
pub(crate) const PDC_MIDI: usize = 140;
pub(crate) const TRIGGER: usize = 141;
pub(crate) const EXT_NOINIT: usize = 142;
pub(crate) const EXT_NODENORM: usize = 143;
const FIRST_USER: usize = 144;

const MAX_DEPTH: u32 = 100;
const MAX_CHAIN: u32 = 500;

/// Variable name → slot table, shared by every section of one effect.
pub(crate) struct Names {
    map: HashMap<String, usize>,
    next: usize,
}

impl Names {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        for n in 0..MAX_CH {
            map.insert(format!("spl{n}"), SPL0 + n);
        }
        for n in 0..MAX_SLIDERS {
            map.insert(format!("slider{}", n + 1), SLIDER1 + n);
        }
        let fixed = [
            ("srate", SRATE),
            ("num_ch", NUM_CH),
            ("samplesblock", SAMPLESBLOCK),
            ("tempo", TEMPO),
            ("play_state", PLAY_STATE),
            ("play_position", PLAY_POSITION),
            ("beat_position", BEAT_POSITION),
            ("ts_num", TS_NUM),
            ("ts_denom", TS_DENOM),
            ("pdc_delay", PDC_DELAY),
            ("pdc_bot_ch", PDC_BOT_CH),
            ("pdc_top_ch", PDC_TOP_CH),
            ("pdc_midi", PDC_MIDI),
            ("trigger", TRIGGER),
            ("ext_noinit", EXT_NOINIT),
            ("ext_nodenorm", EXT_NODENORM),
        ];
        for (n, s) in fixed {
            map.insert(n.to_string(), s);
        }
        Self { map, next: FIRST_USER }
    }

    pub fn slot(&mut self, name: &str) -> usize {
        if let Some(&s) = self.map.get(name) {
            return s;
        }
        let s = self.next;
        self.next += 1;
        self.map.insert(name.to_string(), s);
        s
    }

    /// Makes `name` another name for an existing slot (`slider1:amp=...` aliases).
    pub fn alias(&mut self, name: &str, slot: usize) {
        self.map.insert(name.to_string(), slot);
    }

    pub fn len(&self) -> usize {
        self.next
    }
}

fn builtin(name: &str) -> Option<(Builtin, usize, usize)> {
    use Builtin::*;
    Some(match name {
        "sin" => (Sin, 1, 1),
        "cos" => (Cos, 1, 1),
        "tan" => (Tan, 1, 1),
        "asin" => (Asin, 1, 1),
        "acos" => (Acos, 1, 1),
        "atan" => (Atan, 1, 1),
        "atan2" => (Atan2, 2, 2),
        "sqr" => (Sqr, 1, 1),
        "sqrt" => (Sqrt, 1, 1),
        "pow" => (Pow, 2, 2),
        "exp" => (Exp, 1, 1),
        "log" => (Log, 1, 1),
        "log10" => (Log10, 1, 1),
        "abs" => (Abs, 1, 1),
        "min" => (Min, 2, 2),
        "max" => (Max, 2, 2),
        "sign" => (Sign, 1, 1),
        "floor" => (Floor, 1, 1),
        "ceil" => (Ceil, 1, 1),
        "int" => (Int, 1, 1),
        "rand" => (Rand, 1, 1),
        "invsqrt" => (Invsqrt, 1, 1),
        "memset" => (Memset, 3, 3),
        "memcpy" => (Memcpy, 3, 3),
        "__memtop" => (MemTop, 0, 0),
        "freembuf" => (FreeMBuf, 1, 1),
        _ => return None,
    })
}

/// Binary operator levels, lowest precedence first.
const LEVELS: &[&[(&str, BinOp)]] = &[
    &[("||", BinOp::LOr)],
    &[("&&", BinOp::LAnd)],
    &[
        ("===", BinOp::EqExact),
        ("!==", BinOp::NeExact),
        ("==", BinOp::Eq),
        ("!=", BinOp::Ne),
        ("<=", BinOp::Le),
        (">=", BinOp::Ge),
        ("<", BinOp::Lt),
        (">", BinOp::Gt),
    ],
    &[("&", BinOp::And), ("|", BinOp::Or), ("~", BinOp::Xor)],
    &[("<<", BinOp::Shl), (">>", BinOp::Shr)],
    &[("+", BinOp::Add), ("-", BinOp::Sub)],
    &[("*", BinOp::Mul), ("/", BinOp::Div), ("\\", BinOp::IDiv)],
    &[("%", BinOp::Mod)],
    &[("^", BinOp::Pow)],
];

type R<T> = Result<T, JsfxError>;

/// Compiles one section's source. `first_line` is the file line of its first line (for errors).
pub(crate) fn compile(src: &str, first_line: u32, names: &mut Names) -> R<Expr> {
    let toks = lex(src, first_line)?;
    let mut p = Parser { toks, pos: 0, names, depth: 0 };
    let e = p.parse_seq(None)?;
    if !matches!(p.peek(), Tok::Eof) {
        return Err(p.err("unexpected token"));
    }
    Ok(e)
}

struct Parser<'a> {
    toks: Vec<Lexed>,
    pos: usize,
    names: &'a mut Names,
    depth: u32,
}

static EOF: Tok = Tok::Eof;

impl Parser<'_> {
    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).map_or(&EOF, |l| &l.tok)
    }

    fn line(&self) -> u32 {
        self.toks.get(self.pos).or_else(|| self.toks.last()).map_or(0, |l| l.line)
    }

    fn err(&self, msg: &str) -> JsfxError {
        JsfxError::Syntax(format!("line {}: {msg}", self.line()))
    }

    fn is_p(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::P(q) if *q == p)
    }

    fn eat_p(&mut self, p: &str) -> bool {
        let hit = self.is_p(p);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn expect_p(&mut self, p: &str) -> R<()> {
        if self.eat_p(p) { Ok(()) } else { Err(self.err(&format!("expected '{p}'"))) }
    }

    fn enter(&mut self) -> R<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH { Err(self.err("expression nested too deeply")) } else { Ok(()) }
    }

    fn at_end(&self, close: Option<&str>) -> bool {
        matches!(self.peek(), Tok::Eof) || close.is_some_and(|c| self.is_p(c))
    }

    fn parse_seq(&mut self, close: Option<&str>) -> R<Expr> {
        let mut v = Vec::new();
        loop {
            while self.eat_p(";") {}
            if self.at_end(close) {
                break;
            }
            v.push(self.parse_assign()?);
            if !(self.is_p(";") || self.at_end(close)) {
                return Err(self.err("expected ';'"));
            }
        }
        Ok(if v.len() <= 1 { v.pop().unwrap_or(Expr::Num(0.0)) } else { Expr::Seq(v) })
    }

    fn assign_op(&mut self) -> Option<Option<BinOp>> {
        let Tok::P(p) = self.peek() else { return None };
        let op = match *p {
            "=" => None,
            "+=" => Some(BinOp::Add),
            "-=" => Some(BinOp::Sub),
            "*=" => Some(BinOp::Mul),
            "/=" => Some(BinOp::Div),
            "\\=" => Some(BinOp::IDiv),
            "%=" => Some(BinOp::Mod),
            "^=" => Some(BinOp::Pow),
            "|=" => Some(BinOp::Or),
            "&=" => Some(BinOp::And),
            "~=" => Some(BinOp::Xor),
            _ => return None,
        };
        self.pos += 1;
        Some(op)
    }

    fn parse_assign(&mut self) -> R<Expr> {
        self.enter()?;
        let lhs = self.parse_ternary()?;
        let out = if let Some(op) = self.assign_op() {
            if !matches!(lhs, Expr::Var(_) | Expr::Mem(_) | Expr::GMem(_)) {
                return Err(self.err("cannot assign to this expression"));
            }
            let rhs = self.parse_assign()?;
            Expr::Assign(Box::new(lhs), op, Box::new(rhs))
        } else {
            lhs
        };
        self.depth -= 1;
        Ok(out)
    }

    fn parse_ternary(&mut self) -> R<Expr> {
        let cond = self.parse_bin(0)?;
        if !self.eat_p("?") {
            return Ok(cond);
        }
        let then = self.parse_assign()?;
        let other = if self.eat_p(":") { Some(Box::new(self.parse_assign()?)) } else { None };
        Ok(Expr::Cond(Box::new(cond), Box::new(then), other))
    }

    fn parse_bin(&mut self, level: usize) -> R<Expr> {
        let Some(ops) = LEVELS.get(level) else { return self.parse_unary() };
        let mut lhs = self.parse_bin(level + 1)?;
        let mut n = 0;
        while let Some(&(_, op)) = ops.iter().find(|(s, _)| self.is_p(s)) {
            self.pos += 1;
            n += 1;
            if n > MAX_CHAIN {
                return Err(self.err("expression too long"));
            }
            let rhs = self.parse_bin(level + 1)?;
            lhs = match op {
                BinOp::LAnd => Expr::LogicAnd(Box::new(lhs), Box::new(rhs)),
                BinOp::LOr => Expr::LogicOr(Box::new(lhs), Box::new(rhs)),
                _ => Expr::Bin(op, Box::new(lhs), Box::new(rhs)),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> R<Expr> {
        self.enter()?;
        let out = if self.eat_p("-") {
            match self.parse_unary()? {
                Expr::Num(n) => Expr::Num(-n),
                e => Expr::Unary(UnOp::Neg, Box::new(e)),
            }
        } else if self.eat_p("+") {
            self.parse_unary()?
        } else if self.eat_p("!") {
            Expr::Unary(UnOp::Not, Box::new(self.parse_unary()?))
        } else {
            self.parse_primary()?
        };
        self.depth -= 1;
        Ok(out)
    }

    fn parse_primary(&mut self) -> R<Expr> {
        let tok = self.peek().clone();
        self.pos += 1;
        match tok {
            Tok::Num(n) => Ok(Expr::Num(n)),
            // String literals are only used by string functions, which are rejected as unknown
            // functions; as a plain value they are a harmless 0.
            Tok::Str => Ok(Expr::Num(0.0)),
            Tok::P("(") => {
                let e = self.parse_seq(Some(")"))?;
                self.expect_p(")")?;
                Ok(e)
            }
            Tok::P("[") => {
                let idx = self.parse_assign()?;
                self.expect_p("]")?;
                Ok(Expr::Mem(Box::new(idx)))
            }
            Tok::Ident(name) => self.ident(&name),
            _ => {
                self.pos -= 1;
                Err(self.err("unexpected token in expression"))
            }
        }
    }

    fn ident(&mut self, name: &str) -> R<Expr> {
        if name.starts_with('#') {
            return Err(JsfxError::Unsupported("string variables".into()));
        }
        if name == "function" {
            return Err(JsfxError::Unsupported("user-defined functions".into()));
        }
        if self.is_p("(") {
            self.pos += 1;
            return self.call(name);
        }
        if self.eat_p("[") {
            let idx = if self.is_p("]") { Expr::Num(0.0) } else { self.parse_assign()? };
            self.expect_p("]")?;
            if name == "gmem" {
                return Ok(Expr::GMem(Box::new(idx)));
            }
            let base = Expr::Var(self.names.slot(name));
            return Ok(Expr::Mem(Box::new(Expr::Bin(BinOp::Add, Box::new(base), Box::new(idx)))));
        }
        Ok(Expr::Var(self.names.slot(name)))
    }

    fn call(&mut self, name: &str) -> R<Expr> {
        match name {
            "loop" => {
                let n = self.parse_assign()?;
                self.expect_p(",")?;
                let body = self.parse_seq(Some(")"))?;
                self.expect_p(")")?;
                Ok(Expr::Loop(Box::new(n), Box::new(body)))
            }
            "while" => {
                let cond = self.parse_seq(Some(")"))?;
                self.expect_p(")")?;
                if self.eat_p("(") {
                    let body = self.parse_seq(Some(")"))?;
                    self.expect_p(")")?;
                    Ok(Expr::WhileBody(Box::new(cond), Box::new(body)))
                } else {
                    Ok(Expr::While(Box::new(cond)))
                }
            }
            "if" => {
                let cond = self.parse_assign()?;
                if self.eat_p(",") {
                    let then = self.parse_assign()?;
                    let other = if self.eat_p(",") { Some(Box::new(self.parse_assign()?)) } else { None };
                    self.expect_p(")")?;
                    return Ok(Expr::Cond(Box::new(cond), Box::new(then), other));
                }
                self.expect_p(")")?;
                self.expect_p("(")?;
                let then = self.parse_seq(Some(")"))?;
                self.expect_p(")")?;
                let other = if matches!(self.peek(), Tok::Ident(s) if s == "else") {
                    self.pos += 1;
                    self.expect_p("(")?;
                    let e = self.parse_seq(Some(")"))?;
                    self.expect_p(")")?;
                    Some(Box::new(e))
                } else {
                    None
                };
                Ok(Expr::Cond(Box::new(cond), Box::new(then), other))
            }
            _ => {
                let Some((b, min, max)) = builtin(name) else {
                    return Err(JsfxError::Unsupported(format!("function `{name}`")));
                };
                let mut args = Vec::new();
                if !self.is_p(")") {
                    loop {
                        args.push(self.parse_assign()?);
                        if !self.eat_p(",") {
                            break;
                        }
                    }
                }
                self.expect_p(")")?;
                if args.len() < min || args.len() > max {
                    return Err(self.err(&format!("`{name}` takes {min} argument(s)")));
                }
                Ok(Expr::Call(b, args))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> R<Expr> {
        compile(s, 1, &mut Names::new())
    }

    #[test]
    fn precedence() {
        // 1 + 2 * 3
        let e = parse("1+2*3").unwrap_or(Expr::Num(f64::NAN));
        assert!(matches!(e, Expr::Bin(BinOp::Add, _, r) if matches!(*r, Expr::Bin(BinOp::Mul, _, _))));
        // unary minus binds tighter than ^
        let e = parse("-2^2").unwrap_or(Expr::Num(f64::NAN));
        assert!(matches!(e, Expr::Bin(BinOp::Pow, l, _) if matches!(*l, Expr::Num(n) if n == -2.0)));
    }

    #[test]
    fn unsupported_things_are_named() {
        assert_eq!(parse("x = strlen(y);"), Err(JsfxError::Unsupported("function `strlen`".into())));
        assert!(matches!(parse("function f(x) ( x );"), Err(JsfxError::Unsupported(_))));
    }

    #[test]
    fn syntax_errors_carry_a_line() {
        let Err(JsfxError::Syntax(m)) = parse("a = 1;\nb = ;") else { panic!("expected a syntax error") };
        assert!(m.starts_with("line 2"), "{m}");
    }

    #[test]
    fn deep_nesting_is_rejected_not_overflowed() {
        let s = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        assert!(parse(&s).is_err());
    }

    #[test]
    fn loops_and_conditionals_parse() {
        assert!(parse("loop(4, a += 1; b += 2;); while(i < 3) ( i += 1; ); x ? y = 1 : z = 2;").is_ok());
        assert!(parse("if (x) ( y = 1; ) else ( y = 2; );").is_ok());
    }
}
