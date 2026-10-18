//! Port of `expression/Simplifiable.java`.

/// An expression that can be simplified to an equivalent, but cheaper to evaluate expression.
///
/// Implementers only provide [`Simplifiable::simplify_once`], which applies all the rules that can be used to simplify
/// this expression, and [`Simplifiable::simplify`] applies it repeatedly until the output settles to a fixed point.
/// `PartialEq` must reflect equivalence between expressions so that `simplify` knows when to stop.
pub trait Simplifiable: Sized + Clone + PartialEq {
    /// Returns a copy of this expression, with all simplification rules applied once.
    fn simplify_once(&self) -> Self {
        self.clone()
    }

    /// Returns an equivalent, simplified copy of this expression by repeatedly running
    /// [`Simplifiable::simplify_once`] until reaching a value that was already seen.
    fn simplify(&self) -> Self {
        let mut simplified = self.clone();
        let mut seen = vec![simplified.clone()];
        loop {
            simplified = simplified.simplify_once();
            if seen.contains(&simplified) {
                return simplified;
            }
            if seen.len() > 1000 {
                panic!("Infinite loop while simplifying expression");
            }
            seen.push(simplified.clone());
        }
    }
}
