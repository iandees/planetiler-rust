//! Port of `ScriptEnvironment.java`: type definitions for the environment that a script expression runs in.
//!
//! Java stores a list of CEL variable declarations and the input context class; here the declarations are implied by
//! the [`ContextKind`] (each kind declares the variables of its parents too) and the [`Root`] (which declares `args`
//! and `args.<name>` for every argument).

use std::fmt;

use crate::contexts::{ContextKind, Root};

/// The variables available to an expression, and the kind of context it expects as input.
#[derive(Clone)]
pub struct ScriptEnvironment {
    kind: ContextKind,
    root: Root,
}

impl PartialEq for ScriptEnvironment {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.root == other.root
    }
}

impl fmt::Debug for ScriptEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ScriptEnvironment[{:?}]", self.kind)
    }
}

impl ScriptEnvironment {
    pub fn new(kind: ContextKind, root: Root) -> ScriptEnvironment {
        ScriptEnvironment { kind, root }
    }

    /// The kind of context this environment's expressions take as input.
    pub fn kind(&self) -> ContextKind {
        self.kind
    }

    pub fn root(&self) -> &Root {
        &self.root
    }

    /// Returns a copy of this environment for a new input kind.
    pub fn for_input(&self, kind: ContextKind) -> ScriptEnvironment {
        ScriptEnvironment { kind, root: self.root.clone() }
    }

    /// Returns true if this environment declares `variable`.
    pub fn contains_variable(&self, variable: &str) -> bool {
        self.root.declares(variable) || self.kind.has_feature_variable(variable)
    }
}
