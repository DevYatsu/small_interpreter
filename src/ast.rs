use std::fmt::Debug;

#[derive(Debug, Clone)]
pub enum Statement<'source> {
    Print(Expr<'source>, (usize, usize)),
    ImmutVar(&'source str, Expr<'source>, (usize, usize)),
    MutVar(&'source str, Expr<'source>, (usize, usize)),
}

#[derive(Debug, Clone)]
pub enum Expr<'source> {
    Bool(bool),
    Number(f64),
    String(&'source str),
    Var(&'source str),
}

impl From<f64> for Expr<'_> {
    fn from(value: f64) -> Self {
        Expr::Number(value)
    }
}
