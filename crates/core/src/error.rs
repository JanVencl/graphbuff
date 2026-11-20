use std::fmt;

/// Every error graphbuff can report, from parsing through execution.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The query text could not be tokenized or parsed. `offset` is a byte offset into the query.
    Syntax { message: String, offset: usize },
    /// The query is syntactically valid but semantically wrong (unknown variable, bad clause order, ...).
    Semantic(String),
    /// A value had the wrong type for an operation.
    Type(String),
    /// A graph constraint was violated (e.g. deleting a node that still has relationships).
    Constraint(String),
    /// Any other failure during execution (division by zero, missing entity, ...).
    Runtime(String),
    /// A valid Cypher feature that graphbuff does not implement yet.
    Unsupported(String),
    /// The storage layer failed: an I/O error, a corrupt file, or a store that was
    /// poisoned by an earlier durability failure.
    Storage(String),
    /// A transaction grew past its size cap (persistent storage); it was rolled back.
    TransactionTooLarge(String),
    /// A query ran past its time limit and was stopped; its changes were rolled back.
    Timeout(String),
    /// A query was cancelled while it ran; its changes were rolled back.
    Cancelled(String),
    /// A query held more memory than its limit and was stopped; its changes were rolled
    /// back.
    MemoryLimit(String),
    /// The user running the statement may not run it (see `graphbuff-server`'s user
    /// accounts). Nothing was changed.
    Forbidden(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn syntax(message: impl Into<String>, offset: usize) -> Self {
        Error::Syntax {
            message: message.into(),
            offset,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax { message, offset } => {
                write!(f, "SyntaxError (at offset {offset}): {message}")
            }
            Error::Semantic(m) => write!(f, "SemanticError: {m}"),
            Error::Type(m) => write!(f, "TypeError: {m}"),
            Error::Constraint(m) => write!(f, "ConstraintError: {m}"),
            Error::Runtime(m) => write!(f, "RuntimeError: {m}"),
            Error::Unsupported(m) => write!(f, "Unsupported: {m}"),
            Error::Storage(m) => write!(f, "StorageError: {m}"),
            Error::TransactionTooLarge(m) => write!(f, "TransactionTooLarge: {m}"),
            Error::Timeout(m) => write!(f, "Timeout: {m}"),
            Error::Cancelled(m) => write!(f, "Cancelled: {m}"),
            Error::MemoryLimit(m) => write!(f, "MemoryLimit: {m}"),
            Error::Forbidden(m) => write!(f, "Forbidden: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Wraps an I/O error with a short description of what was being done.
    pub fn io(context: impl fmt::Display, e: std::io::Error) -> Self {
        Error::Storage(format!("{context}: {e}"))
    }
}
