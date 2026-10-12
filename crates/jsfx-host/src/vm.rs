//! Tree-walking EEL2 interpreter. Never allocates while evaluating, never panics: memory accesses are
//! bounds-checked (out-of-range reads give 0, writes are dropped) and every loop iteration draws on
//! a fuel budget so a runaway script is cut off instead of hanging the audio thread.

use crate::ast::{BinOp, Builtin, Expr, UnOp};

/// EEL2 treats values closer to zero than this as false and equal.
const EPS: f64 = 1e-5;
/// EEL2 caps `loop` counts and `while` iterations at this many.
const MAX_ITER: usize = 1 << 20;

pub(crate) fn truthy(x: f64) -> bool {
    x.abs() > EPS
}

fn to_i(x: f64) -> i64 {
    // Saturating; NaN becomes 0.
    x as i64
}

fn b2f(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

pub(crate) fn binop(op: BinOp, a: f64, b: f64) -> f64 {
    match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b == 0.0 {
                0.0
            } else {
                a / b
            }
        }
        BinOp::IDiv => {
            let d = to_i(b);
            if d == 0 { 0.0 } else { to_i(a).wrapping_div(d) as f64 }
        }
        BinOp::Mod => {
            let d = to_i(b);
            if d == 0 { 0.0 } else { to_i(a).wrapping_rem(d) as f64 }
        }
        BinOp::Pow => a.powf(b),
        BinOp::Shl => {
            let s = to_i(b);
            if (0..64).contains(&s) { to_i(a).wrapping_shl(s as u32) as f64 } else { 0.0 }
        }
        BinOp::Shr => {
            let s = to_i(b);
            if (0..64).contains(&s) { to_i(a).wrapping_shr(s as u32) as f64 } else { 0.0 }
        }
        BinOp::And => (to_i(a) & to_i(b)) as f64,
        BinOp::Or => (to_i(a) | to_i(b)) as f64,
        BinOp::Xor => (to_i(a) ^ to_i(b)) as f64,
        BinOp::Eq => b2f((a - b).abs() < EPS),
        BinOp::Ne => b2f((a - b).abs() >= EPS),
        BinOp::EqExact => b2f(a == b),
        BinOp::NeExact => b2f(a != b),
        BinOp::Lt => b2f(a < b),
        BinOp::Le => b2f(a <= b),
        BinOp::Gt => b2f(a > b),
        BinOp::Ge => b2f(a >= b),
        // Handled with short-circuiting in `eval`; kept total for completeness.
        BinOp::LAnd => b2f(truthy(a) && truthy(b)),
        BinOp::LOr => b2f(truthy(a) || truthy(b)),
    }
}

/// Slot for a memory index, `None` when out of range.
fn index(v: f64, len: usize) -> Option<usize> {
    let i = v + 0.0001;
    if i >= 0.0 && i < len as f64 { Some(i as usize) } else { None }
}

pub(crate) struct Machine {
    pub vars: Vec<f64>,
    mem: Vec<f64>,
    gmem: Vec<f64>,
    rng: u64,
    fuel: u64,
    out_of_fuel: bool,
}

impl Machine {
    pub fn new(nvars: usize, mem_len: usize) -> Self {
        Self { vars: vec![0.0; nvars], mem: vec![0.0; mem_len], gmem: vec![0.0; 1 << 16], rng: 0x9E37_79B9_7F4A_7C15, fuel: 0, out_of_fuel: false }
    }

    /// Zeroes all variables and memory (memory is reallocated zeroed, which the OS provides lazily).
    pub fn reset(&mut self) {
        self.vars.fill(0.0);
        self.mem = vec![0.0; self.mem.len()];
        self.gmem.fill(0.0);
    }

    pub fn var(&self, slot: usize) -> f64 {
        self.vars.get(slot).copied().unwrap_or(0.0)
    }

    pub fn set_var(&mut self, slot: usize, v: f64) {
        if let Some(x) = self.vars.get_mut(slot) {
            *x = v;
        }
    }

    /// Runs `e` with a budget of `fuel` loop iterations. `false` when the budget ran out.
    pub fn run(&mut self, e: &Expr, fuel: u64) -> bool {
        self.fuel = fuel;
        self.out_of_fuel = false;
        self.eval(e);
        !self.out_of_fuel
    }

    fn tick(&mut self) -> bool {
        if self.fuel == 0 {
            self.out_of_fuel = true;
            false
        } else {
            self.fuel -= 1;
            true
        }
    }

    fn rand01(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    fn mem_get(&self, i: f64) -> f64 {
        index(i, self.mem.len()).and_then(|k| self.mem.get(k)).copied().unwrap_or(0.0)
    }

    fn gmem_get(&self, i: f64) -> f64 {
        index(i, self.gmem.len()).and_then(|k| self.gmem.get(k)).copied().unwrap_or(0.0)
    }

    fn store(&mut self, target: &Expr, idx: f64, v: f64) {
        let slot = match target {
            Expr::Mem(_) => index(idx, self.mem.len()).and_then(|k| self.mem.get_mut(k)),
            _ => index(idx, self.gmem.len()).and_then(|k| self.gmem.get_mut(k)),
        };
        if let Some(s) = slot {
            *s = v;
        }
    }

    fn eval(&mut self, e: &Expr) -> f64 {
        match e {
            Expr::Num(n) => *n,
            Expr::Var(s) => self.var(*s),
            Expr::Mem(i) => {
                let i = self.eval(i);
                self.mem_get(i)
            }
            Expr::GMem(i) => {
                let i = self.eval(i);
                self.gmem_get(i)
            }
            Expr::Unary(UnOp::Neg, x) => -self.eval(x),
            Expr::Unary(UnOp::Not, x) => b2f(!truthy(self.eval(x))),
            Expr::Bin(op, a, b) => {
                let a = self.eval(a);
                let b = self.eval(b);
                binop(*op, a, b)
            }
            Expr::LogicAnd(a, b) => b2f(truthy(self.eval(a)) && truthy(self.eval(b))),
            Expr::LogicOr(a, b) => b2f(truthy(self.eval(a)) || truthy(self.eval(b))),
            Expr::Cond(c, t, f) => {
                if truthy(self.eval(c)) {
                    self.eval(t)
                } else if let Some(f) = f {
                    self.eval(f)
                } else {
                    0.0
                }
            }
            Expr::Assign(target, op, rhs) => self.assign(target, *op, rhs),
            Expr::Seq(v) => {
                let mut last = 0.0;
                for x in v {
                    last = self.eval(x);
                }
                last
            }
            Expr::Call(b, args) => self.call(*b, args),
            Expr::Loop(n, body) => {
                let n = self.eval(n);
                let n = if n >= 1.0 { (n as usize).min(MAX_ITER) } else { 0 };
                for _ in 0..n {
                    if !self.tick() {
                        break;
                    }
                    self.eval(body);
                }
                0.0
            }
            Expr::While(cond) => {
                for _ in 0..MAX_ITER {
                    if !self.tick() || !truthy(self.eval(cond)) {
                        break;
                    }
                }
                0.0
            }
            Expr::WhileBody(cond, body) => {
                for _ in 0..MAX_ITER {
                    if !self.tick() || !truthy(self.eval(cond)) {
                        break;
                    }
                    self.eval(body);
                }
                0.0
            }
        }
    }

    fn assign(&mut self, target: &Expr, op: Option<BinOp>, rhs: &Expr) -> f64 {
        match target {
            Expr::Var(s) => {
                let r = self.eval(rhs);
                let v = op.map_or(r, |o| binop(o, self.var(*s), r));
                self.set_var(*s, v);
                v
            }
            Expr::Mem(i) | Expr::GMem(i) => {
                let idx = self.eval(i);
                let r = self.eval(rhs);
                let v = match op {
                    None => r,
                    Some(o) => {
                        let cur = if matches!(target, Expr::Mem(_)) { self.mem_get(idx) } else { self.gmem_get(idx) };
                        binop(o, cur, r)
                    }
                };
                self.store(target, idx, v);
                v
            }
            _ => 0.0,
        }
    }

    fn call(&mut self, b: Builtin, args: &[Expr]) -> f64 {
        let a0 = args.first().map_or(0.0, |e| self.eval(e));
        let a1 = args.get(1).map_or(0.0, |e| self.eval(e));
        let a2 = args.get(2).map_or(0.0, |e| self.eval(e));
        match b {
            Builtin::Sin => a0.sin(),
            Builtin::Cos => a0.cos(),
            Builtin::Tan => a0.tan(),
            Builtin::Asin => a0.asin(),
            Builtin::Acos => a0.acos(),
            Builtin::Atan => a0.atan(),
            Builtin::Atan2 => a0.atan2(a1),
            Builtin::Sqr => a0 * a0,
            Builtin::Sqrt => a0.abs().sqrt(),
            Builtin::Pow => a0.powf(a1),
            Builtin::Exp => a0.exp(),
            Builtin::Log => a0.ln(),
            Builtin::Log10 => a0.log10(),
            Builtin::Abs => a0.abs(),
            Builtin::Min => a0.min(a1),
            Builtin::Max => a0.max(a1),
            Builtin::Sign => {
                if a0 > 0.0 {
                    1.0
                } else if a0 < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            }
            Builtin::Floor => a0.floor(),
            Builtin::Ceil => a0.ceil(),
            Builtin::Int => a0.trunc(),
            Builtin::Rand => a0 * self.rand01(),
            Builtin::Invsqrt => {
                if a0 > 0.0 {
                    1.0 / a0.sqrt()
                } else {
                    0.0
                }
            }
            Builtin::Memset => {
                if let Some(d) = index(a0, self.mem.len()) {
                    let n = if a2 > 0.0 { (a2 as usize).min(self.mem.len() - d) } else { 0 };
                    if let Some(s) = self.mem.get_mut(d..d + n) {
                        s.fill(a1);
                    }
                }
                a0
            }
            Builtin::Memcpy => {
                if let (Some(d), Some(s)) = (index(a0, self.mem.len()), index(a1, self.mem.len())) {
                    let n = if a2 > 0.0 { (a2 as usize).min(self.mem.len() - d).min(self.mem.len() - s) } else { 0 };
                    if n > 0 {
                        self.mem.copy_within(s..s + n, d);
                    }
                }
                a0
            }
            Builtin::MemTop => self.mem.len() as f64,
            Builtin::FreeMBuf => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{Names, compile};

    fn run(src: &str) -> (Machine, Names) {
        let mut names = Names::new();
        let e = compile(src, 1, &mut names).unwrap();
        let mut m = Machine::new(names.len(), 1024);
        m.run(&e, 1_000_000);
        (m, names)
    }

    fn val(src: &str, var: &str) -> f64 {
        let (m, mut names) = run(src);
        m.var(names.slot(var))
    }

    #[test]
    fn arithmetic_and_assignment() {
        assert_eq!(val("x = 1 + 2 * 3;", "x"), 7.0);
        assert_eq!(val("x = 2; x *= 5; x -= 1;", "x"), 9.0);
        assert_eq!(val("x = 7 % 4; y = 7 / 0;", "x"), 3.0);
        assert_eq!(val("x = 7 / 0;", "x"), 0.0);
        assert_eq!(val("x = 5 & 3 | 8;", "x"), 9.0);
        assert_eq!(val("x = 1 << 4;", "x"), 16.0);
    }

    #[test]
    fn comparisons_use_epsilon_and_logic_short_circuits() {
        assert_eq!(val("x = 1 == 1.000001;", "x"), 1.0);
        assert_eq!(val("x = 1 === 1.000001;", "x"), 0.0);
        assert_eq!(val("x = 0 && (y = 5);", "y"), 0.0);
        assert_eq!(val("x = 1 || (y = 5);", "y"), 0.0);
        assert_eq!(val("x = !0;", "x"), 1.0);
    }

    #[test]
    fn control_flow() {
        assert_eq!(val("i = 0; loop(10, i += 2;);", "i"), 20.0);
        assert_eq!(val("i = 0; while(i < 5) ( i += 1; );", "i"), 5.0);
        assert_eq!(val("i = 0; while(i += 1; i < 7);", "i"), 7.0);
        assert_eq!(val("x = 3 > 2 ? 10 : 20;", "x"), 10.0);
        assert_eq!(val("if (1) ( x = 4; ) else ( x = 9; );", "x"), 4.0);
    }

    #[test]
    fn memory_is_offset_by_the_base_variable() {
        assert_eq!(val("buf = 100; buf[3] = 7; x = mem[103];", "x"), 7.0);
        assert_eq!(val("buf = 10; buf[0] = 1; buf[1] = 2; memcpy(buf + 5, buf, 2); x = buf[6];", "x"), 2.0);
        assert_eq!(val("memset(0, 3, 4); x = mem[0] + mem[3] + mem[4];", "x"), 6.0);
        assert_eq!(val("gmem[2] = 5; x = gmem[2];", "x"), 5.0);
    }

    #[test]
    fn out_of_range_memory_is_harmless() {
        assert_eq!(val("mem[-5] = 1; mem[99999999999] = 1; x = mem[-5] + mem[1e300];", "x"), 0.0);
        assert_eq!(val("x = mem[0/0];", "x"), 0.0);
        assert_eq!(val("memset(1000, 1, 1e12); memcpy(1000, 0, 1e12); x = 1;", "x"), 1.0);
    }

    #[test]
    fn runaway_loops_are_cut_off() {
        let mut names = Names::new();
        let e = compile("while(1) ( x += 1; );", 1, &mut names).unwrap();
        let mut m = Machine::new(names.len(), 16);
        assert!(!m.run(&e, 1000));
        let e = compile("loop(1e9, loop(1e9, x += 1;););", 1, &mut names).unwrap();
        assert!(!m.run(&e, 5000));
    }

    #[test]
    fn math_builtins() {
        assert!((val("x = sin($pi/2);", "x") - 1.0).abs() < 1e-12);
        assert_eq!(val("x = max(3, min(9, 5));", "x"), 5.0);
        assert_eq!(val("x = sign(-3) + int(2.9) + floor(-0.5);", "x"), 0.0);
        assert_eq!(val("x = sqrt(-4);", "x"), 2.0);
        assert!(val("x = rand(1);", "x") < 1.0);
    }
}
