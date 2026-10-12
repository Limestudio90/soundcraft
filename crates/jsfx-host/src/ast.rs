//! The EEL2 expression tree.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Shl,
    Shr,
    And,
    Or,
    Xor,
    Eq,
    Ne,
    EqExact,
    NeExact,
    Lt,
    Le,
    Gt,
    Ge,
    /// Short-circuit operators; only used by the parser to build `LogicAnd`/`LogicOr`.
    LAnd,
    LOr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Sqr,
    Sqrt,
    Pow,
    Exp,
    Log,
    Log10,
    Abs,
    Min,
    Max,
    Sign,
    Floor,
    Ceil,
    Int,
    Rand,
    Invsqrt,
    Memset,
    Memcpy,
    MemTop,
    FreeMBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Num(f64),
    Var(usize),
    /// `mem[index]` (the index already includes the base variable).
    Mem(Box<Expr>),
    GMem(Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    LogicAnd(Box<Expr>, Box<Expr>),
    LogicOr(Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Option<Box<Expr>>),
    /// Target is `Var`, `Mem` or `GMem`; the operator is `None` for plain `=`.
    Assign(Box<Expr>, Option<BinOp>, Box<Expr>),
    Seq(Vec<Expr>),
    Call(Builtin, Vec<Expr>),
    Loop(Box<Expr>, Box<Expr>),
    While(Box<Expr>),
    WhileBody(Box<Expr>, Box<Expr>),
}
