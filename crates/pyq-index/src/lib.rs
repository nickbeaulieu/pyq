//! `pyq-index` — parse Python and extract queryable facts.
//!
//! The premise of pyq: expose code-as-graph as composable data an agent can
//! query for ground truth, rather than re-deriving it by grepping. This crate
//! owns the parse + extraction; query verbs live in `pyq-cli`.

pub mod extract;
pub mod model;

pub use extract::extract;
pub use model::{Def, DefKind, FileIndex, ImportStmt, Input, InputKind, Pos, Ref};
