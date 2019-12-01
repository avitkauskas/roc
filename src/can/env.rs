use crate::can::problem::Problem;
use crate::can::procedure::References;
use crate::can::symbol::Symbol;
use crate::collections::{ImMap, MutMap};
use crate::region::Located;

/// The canonicalization environment for a particular module.
pub struct Env {
    /// The module's path. Unqualified references to identifiers and variant names are assumed
    /// to be relative to this path.
    pub home: Box<str>,

    /// Problems we've encountered along the way, which will be reported to the user at the end.
    pub problems: Vec<Problem>,

    /// Variants either declared in this module, or imported.
    pub variants: ImMap<Symbol, Located<Box<str>>>,

    /// Closures
    pub closures: MutMap<Symbol, References>,

    /// current tail-callable symbol
    pub tailcallable_symbol: Option<Symbol>,
}

impl Env {
    pub fn new(home: Box<str>, declared_variants: ImMap<Symbol, Located<Box<str>>>) -> Env {
        Env {
            home,
            variants: declared_variants,
            problems: Vec::new(),
            closures: MutMap::default(),
            tailcallable_symbol: None,
        }
    }

    pub fn problem(&mut self, problem: Problem) {
        self.problems.push(problem)
    }

    pub fn register_closure(&mut self, symbol: Symbol, references: References) {
        self.closures.insert(symbol, references);
    }
}
