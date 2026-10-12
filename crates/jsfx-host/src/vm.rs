//! Tree-walking EEL2 interpreter. Never allocates while evaluating, never panics: memory accesses are
//! bounds-checked (out-of-range reads give 0, writes are dropped) and every loop iteration draws on
//! a fuel budget so a runaway script is cut off instead of hanging the audio thread.

use crate::ast::{BinOp, Builtin, Expr, Func, UnOp};
use crate::fft;
use crate::parse::{MAX_SLIDERS, SLIDER1};

/// EEL2 treats values closer to zero than this as false and equal.
const EPS: f64 = 1e-5;
/// EEL2 caps `loop` counts and `while` iterations at this many.
const MAX_ITER: usize = 1 << 20;
/// Most arguments a user function call can carry.
pub(crate) const MAX_ARGS: usize = 32;
/// Pending MIDI events kept per block; extra events are dropped.
const MIDI_CAP: usize = 1024;

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

/// Where an assignment lands.
#[derive(Clone, Copy)]
enum Loc {
    Var(usize),
    Mem(usize),
    GMem(usize),
    Nowhere,
}

#[derive(Clone, Copy, Default)]
struct MidiEvent {
    offset: u32,
    msg: [u8; 3],
}

pub(crate) struct Machine {
    pub vars: Vec<f64>,
    mem: Vec<f64>,
    gmem: Vec<f64>,
    rng: u64,
    fuel: u64,
    out_of_fuel: bool,
    /// Sliders changed since `sliderchange` last looked (bit n-1 for slider n).
    changed: u64,
    midi: Vec<MidiEvent>,
    midi_head: usize,
}

impl Machine {
    pub fn new(nvars: usize, mem_len: usize) -> Self {
        Self {
            vars: vec![0.0; nvars],
            mem: vec![0.0; mem_len],
            gmem: vec![0.0; 1 << 16],
            rng: 0x9E37_79B9_7F4A_7C15,
            fuel: 0,
            out_of_fuel: false,
            changed: u64::MAX,
            midi: Vec::with_capacity(MIDI_CAP),
            midi_head: 0,
        }
    }

    /// Zeroes all variables and memory (memory is reallocated zeroed, which the OS provides lazily).
    pub fn reset(&mut self) {
        self.vars.fill(0.0);
        self.mem = vec![0.0; self.mem.len()];
        self.gmem.fill(0.0);
        self.changed = u64::MAX;
        self.clear_midi();
    }

    pub fn var(&self, slot: usize) -> f64 {
        self.vars.get(slot).copied().unwrap_or(0.0)
    }

    pub fn set_var(&mut self, slot: usize, v: f64) {
        if let Some(x) = self.vars.get_mut(slot) {
            *x = v;
        }
    }

    /// Notes that slider `n` (1-based) changed, for `sliderchange`.
    pub fn mark_slider_changed(&mut self, n: usize) {
        if (1..=MAX_SLIDERS).contains(&n) {
            self.changed |= 1u64 << (n - 1);
        }
    }

    /// Queues a MIDI event for `midirecv`; dropped when the block's queue is full.
    pub fn push_midi(&mut self, offset: usize, status: u8, d1: u8, d2: u8) {
        if self.midi.len() < MIDI_CAP {
            self.midi.push(MidiEvent { offset: offset.min(u32::MAX as usize) as u32, msg: [status, d1, d2] });
        }
    }

    pub fn clear_midi(&mut self) {
        self.midi.clear();
        self.midi_head = 0;
    }

    /// Runs `e` with a budget of `fuel` loop iterations. `false` when the budget ran out.
    pub fn run(&mut self, e: &Expr, funcs: &[Func], fuel: u64) -> bool {
        self.fuel = fuel;
        self.out_of_fuel = false;
        self.eval(e, funcs);
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

    fn locate(&mut self, target: &Expr, f: &[Func]) -> Loc {
        match target {
            Expr::Var(s) => Loc::Var(*s),
            Expr::VarIdx { base, first, last, idx } => {
                let i = to_i(self.eval(idx, f)).clamp(*first, *last);
                Loc::Var((*base as i64 + (i - first)) as usize)
            }
            Expr::Mem(i) => {
                let i = self.eval(i, f);
                index(i, self.mem.len()).map_or(Loc::Nowhere, Loc::Mem)
            }
            Expr::GMem(i) => {
                let i = self.eval(i, f);
                index(i, self.gmem.len()).map_or(Loc::Nowhere, Loc::GMem)
            }
            _ => Loc::Nowhere,
        }
    }

    fn load(&self, loc: Loc) -> f64 {
        match loc {
            Loc::Var(s) => self.var(s),
            Loc::Mem(k) => self.mem.get(k).copied().unwrap_or(0.0),
            Loc::GMem(k) => self.gmem.get(k).copied().unwrap_or(0.0),
            Loc::Nowhere => 0.0,
        }
    }

    fn save(&mut self, loc: Loc, v: f64) {
        match loc {
            Loc::Var(s) => self.set_var(s, v),
            Loc::Mem(k) => {
                if let Some(x) = self.mem.get_mut(k) {
                    *x = v;
                }
            }
            Loc::GMem(k) => {
                if let Some(x) = self.gmem.get_mut(k) {
                    *x = v;
                }
            }
            Loc::Nowhere => {}
        }
    }

    fn eval(&mut self, e: &Expr, f: &[Func]) -> f64 {
        match e {
            Expr::Num(n) => *n,
            Expr::Var(s) => self.var(*s),
            Expr::VarIdx { .. } | Expr::Mem(_) | Expr::GMem(_) => {
                let loc = self.locate(e, f);
                self.load(loc)
            }
            Expr::Unary(UnOp::Neg, x) => -self.eval(x, f),
            Expr::Unary(UnOp::Not, x) => b2f(!truthy(self.eval(x, f))),
            Expr::Bin(op, a, b) => {
                let a = self.eval(a, f);
                let b = self.eval(b, f);
                binop(*op, a, b)
            }
            Expr::LogicAnd(a, b) => b2f(truthy(self.eval(a, f)) && truthy(self.eval(b, f))),
            Expr::LogicOr(a, b) => b2f(truthy(self.eval(a, f)) || truthy(self.eval(b, f))),
            Expr::Cond(c, t, o) => {
                if truthy(self.eval(c, f)) {
                    self.eval(t, f)
                } else if let Some(o) = o {
                    self.eval(o, f)
                } else {
                    0.0
                }
            }
            Expr::Assign(target, op, rhs) => {
                let r = self.eval(rhs, f);
                let loc = self.locate(target, f);
                let v = op.map_or(r, |o| binop(o, self.load(loc), r));
                self.save(loc, v);
                v
            }
            Expr::Seq(v) => {
                let mut last = 0.0;
                for x in v {
                    last = self.eval(x, f);
                }
                last
            }
            Expr::Call(b, args) => self.call(*b, args, f),
            Expr::UserCall(id, args) => {
                let mut tmp = [0.0f64; MAX_ARGS];
                for (slot, a) in tmp.iter_mut().zip(args) {
                    *slot = self.eval(a, f);
                }
                let Some(func) = f.get(*id) else { return 0.0 };
                for (slot, v) in func.params.iter().zip(tmp) {
                    self.set_var(*slot, v);
                }
                self.eval(&func.body, f)
            }
            Expr::Loop(n, body) => {
                let n = self.eval(n, f);
                let n = if n >= 1.0 { (n as usize).min(MAX_ITER) } else { 0 };
                for _ in 0..n {
                    if !self.tick() {
                        break;
                    }
                    self.eval(body, f);
                }
                0.0
            }
            Expr::While(cond) => {
                for _ in 0..MAX_ITER {
                    if !self.tick() || !truthy(self.eval(cond, f)) {
                        break;
                    }
                }
                0.0
            }
            Expr::WhileBody(cond, body) => {
                for _ in 0..MAX_ITER {
                    if !self.tick() || !truthy(self.eval(cond, f)) {
                        break;
                    }
                    self.eval(body, f);
                }
                0.0
            }
        }
    }

    /// The complex buffer `fft`-style functions work on: `n` points starting at `start`.
    fn fft_buffer(&mut self, start: f64, n: f64) -> Option<&mut [f64]> {
        let n = fft::points(n)?;
        let s = index(start, self.mem.len())?;
        self.mem.get_mut(s..s.checked_add(2 * n)?)
    }

    fn call(&mut self, b: Builtin, args: &[Expr], f: &[Func]) -> f64 {
        match b {
            Builtin::SliderChange => {
                let mask = match args.first() {
                    Some(Expr::Var(s)) if (SLIDER1..SLIDER1 + MAX_SLIDERS).contains(s) => 1u64 << (s - SLIDER1),
                    Some(a) => to_i(self.eval(a, f)) as u64,
                    None => 0,
                };
                let hit = self.changed & mask != 0;
                self.changed &= !mask;
                return b2f(hit);
            }
            Builtin::MidiRecv => {
                let Some(ev) = self.midi.get(self.midi_head).copied() else { return 0.0 };
                self.midi_head += 1;
                let off = f64::from(ev.offset);
                let [status, d1, d2] = ev.msg.map(f64::from);
                // The 3-argument form packs both data bytes into one value: `msg23 = d1 + d2 * 256`.
                let vals = if args.len() == 3 { [off, status, d1 + d2 * 256.0, 0.0] } else { [off, status, d1, d2] };
                for (a, v) in args.iter().zip(vals) {
                    let loc = self.locate(a, f);
                    self.save(loc, v);
                }
                return 1.0;
            }
            _ => {}
        }
        let a0 = args.first().map_or(0.0, |e| self.eval(e, f));
        let a1 = args.get(1).map_or(0.0, |e| self.eval(e, f));
        let a2 = args.get(2).map_or(0.0, |e| self.eval(e, f));
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
            Builtin::FreeMBuf | Builtin::Ignored => 0.0,
            Builtin::Fft => {
                if let Some(buf) = self.fft_buffer(a0, a1) {
                    fft::fft(buf);
                }
                0.0
            }
            Builtin::Ifft => {
                if let Some(buf) = self.fft_buffer(a0, a1) {
                    fft::ifft(buf);
                }
                0.0
            }
            Builtin::FftPermute | Builtin::FftIpermute => {
                if let Some(buf) = self.fft_buffer(a0, a1) {
                    fft::permute(buf);
                }
                0.0
            }
            // Handled before the arguments are evaluated.
            Builtin::SliderChange | Builtin::MidiRecv => 0.0,
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
        m.run(&e, &names.funcs, 1_000_000);
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
        assert_eq!(val("buf[2] += 4; buf[2] *= 3; x = buf[2];", "x"), 12.0);
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
        assert!(!m.run(&e, &names.funcs, 1000));
        let e = compile("loop(1e9, loop(1e9, x += 1;););", 1, &mut names).unwrap();
        assert!(!m.run(&e, &names.funcs, 5000));
    }

    #[test]
    fn math_builtins() {
        assert!((val("x = sin($pi/2);", "x") - 1.0).abs() < 1e-12);
        assert_eq!(val("x = max(3, min(9, 5));", "x"), 5.0);
        assert_eq!(val("x = sign(-3) + int(2.9) + floor(-0.5);", "x"), 0.0);
        assert_eq!(val("x = sqrt(-4);", "x"), 2.0);
        assert!(val("x = rand(1);", "x") < 1.0);
    }

    #[test]
    fn user_functions_have_params_locals_and_a_return_value() {
        assert_eq!(val("function sq(a) ( a * a; ); x = sq(7);", "x"), 49.0);
        assert_eq!(val("function f(a, b) local(t) ( t = a - b; t * 2; ); x = f(10, 4);", "x"), 12.0);
        // Arguments are evaluated before any parameter is written, so nesting a call is safe.
        assert_eq!(val("function f(a, b) ( a * 10 + b; ); x = f(f(1, 2), f(3, 4));", "x"), 154.0);
        // Calling again reuses the instance; later definitions can call earlier ones.
        assert_eq!(val("function a(x) ( x + 1; ); function b(x) ( a(a(x)); ); y = b(5) + b(0);", "y"), 9.0);
    }

    #[test]
    fn function_variables_are_local_unless_declared_global() {
        assert_eq!(val("function f() ( g = 5; ); f(); x = g;", "x"), 0.0);
        assert_eq!(val("function f() global(g) ( g = 5; ); f(); x = g;", "x"), 5.0);
        // Locals keep per-function storage, separate from a global of the same name.
        assert_eq!(val("t = 3; function f() local(t) ( t = 99; ); f(); x = t;", "x"), 3.0);
    }

    #[test]
    fn instance_variables_live_in_a_namespace() {
        let src = "function inc() instance(n) ( n += 1; ); a.inc(); a.inc(); b.inc(); x = a.n; y = b.n;";
        assert_eq!(val(src, "x"), 2.0);
        assert_eq!(val(src, "y"), 1.0);
        let src = "function add(v) instance(sum) ( this.sum += v; ); p.add(3); p.add(4); x = p.sum;";
        assert_eq!(val(src, "x"), 7.0);
        let src = "function inner() instance(k) ( k += 10; ); function outer() instance(j) ( inner(); j += 1; ); q.outer(); x = q.k + q.j;";
        assert_eq!(val(src, "x"), 11.0);
    }

    #[test]
    fn recursion_and_unknown_calls_are_compile_errors() {
        let mut names = Names::new();
        assert!(compile("function f(x) ( f(x); ); f(1);", 1, &mut names).is_err());
        assert!(compile("function g(x) ( x ); g(1, 2);", 1, &mut names).is_err());
    }

    #[test]
    fn char_constants_parenthesised_indexing_and_short_calls() {
        assert_eq!(val("x = 'b'; y = $'b';", "x"), 98.0);
        assert_eq!(val("x = 'ab';", "x"), (97 * 256 + 98) as f64);
        assert_eq!(val("mem[7] = 3; n = 4; x = ((n + 1) * 1)[2];", "x"), 3.0);
        // A call with fewer arguments than parameters zeroes the missing ones.
        assert_eq!(val("function f(a, b) ( a + b + 100; ); x = f(1);", "x"), 101.0);
        // A function named with a dot is reachable through a namespace.
        assert_eq!(val("function m.set(v) instance(s) ( s = v; ); p.m.set(5); x = p.s;", "x"), 5.0);
    }

    #[test]
    fn spl_and_slider_functions_are_places() {
        assert_eq!(val("spl(3) = 5; x = spl3;", "x"), 5.0);
        assert_eq!(val("slider2 = 8; x = slider(2);", "x"), 8.0);
        assert_eq!(val("spl(999) = 1; x = spl63;", "x"), 1.0);
    }

    #[test]
    fn sliderchange_reports_each_change_once() {
        let mut names = Names::new();
        let e = compile("a = sliderchange(slider2); b = sliderchange(slider2); c = sliderchange(1);", 1, &mut names).unwrap();
        let mut m = Machine::new(names.len(), 16);
        m.changed = 0;
        m.mark_slider_changed(2);
        m.run(&e, &names.funcs, 1000);
        assert_eq!((m.var(names.slot("a")), m.var(names.slot("b")), m.var(names.slot("c"))), (1.0, 0.0, 0.0));
    }

    #[test]
    fn midirecv_pops_queued_events_into_its_arguments() {
        let mut names = Names::new();
        let e = compile("while (midirecv(t, m1, m2, m3)) ( n += 1; sum += m2; last = m1; );", 1, &mut names).unwrap();
        let mut m = Machine::new(names.len(), 16);
        m.push_midi(4, 0x90, 60, 100);
        m.push_midi(9, 0x80, 61, 0);
        m.run(&e, &names.funcs, 1000);
        assert_eq!((m.var(names.slot("n")), m.var(names.slot("sum")), m.var(names.slot("last"))), (2.0, 121.0, 128.0));
        m.clear_midi();
        m.run(&e, &names.funcs, 1000);
        assert_eq!(m.var(names.slot("n")), 2.0);
    }

    #[test]
    fn fft_round_trips_through_memory() {
        let src = "i = 0; loop(16, buf[i*2] = i; i += 1;); fft(0, 16); fft_permute(0, 16); dc = buf[0]; ifft(0, 16);";
        let mut names = Names::new();
        // fft_permute after fft makes natural order; ifft then expects permuted order again.
        let e = compile(&src.replace("ifft(0, 16);", "fft_ipermute(0, 16); ifft(0, 16); back = buf[10];"), 1, &mut names).unwrap();
        let mut m = Machine::new(names.len(), 256);
        m.run(&e, &names.funcs, 100_000);
        assert_eq!(m.var(names.slot("dc")), 120.0);
        assert!((m.var(names.slot("back")) / 16.0 - 5.0).abs() < 1e-9);
    }
}
