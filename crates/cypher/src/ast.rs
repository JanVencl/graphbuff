//! Abstract syntax tree for the supported Cypher subset.

pub use graphbuff_core::{ConstraintKind, IndexEntity};
use graphbuff_core::Value;

/// A full statement: one or more single queries joined by `UNION` / `UNION ALL`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub parts: Vec<SingleQuery>,
    /// `true` for `UNION ALL` (keep duplicates). Mixing both forms is rejected by the parser.
    pub union_all: bool,
    /// `EXPLAIN` / `PROFILE` prefix.
    pub mode: QueryMode,
}

/// How a query runs: normally, only planned (`EXPLAIN`), or run while measuring each
/// operator (`PROFILE`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueryMode {
    #[default]
    Run,
    Explain,
    Profile,
}

impl Query {
    /// Whether any part changes the graph (also inside `CALL { … }`).
    pub fn is_updating(&self) -> bool {
        self.parts.iter().any(|p| p.clauses.iter().any(Clause::is_updating))
    }

    /// Whether it has a `CALL { … } IN TRANSACTIONS` clause, which commits transactions of
    /// its own (and so cannot run inside an explicit transaction).
    pub fn calls_in_transactions(&self) -> bool {
        self.parts
            .iter()
            .flat_map(|p| &p.clauses)
            .any(|c| matches!(c, Clause::CallSubquery { in_transactions: Some(_), .. }))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SingleQuery {
    pub clauses: Vec<Clause>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match {
        optional: bool,
        patterns: Vec<PatternPart>,
        where_: Option<Expr>,
    },
    Create {
        patterns: Vec<PatternPart>,
    },
    Merge {
        pattern: PatternPart,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    Set(Vec<SetItem>),
    Remove(Vec<RemoveItem>),
    Delete {
        detach: bool,
        exprs: Vec<Expr>,
    },
    Unwind {
        expr: Expr,
        var: String,
    },
    With(Projection),
    Return(Projection),
    /// `CALL { … }`, `CALL (a, b) { … }`, `CALL (*) { … }`: runs the subquery once per
    /// row. A subquery ending in `RETURN` adds its columns (a row per subquery row);
    /// otherwise (a unit subquery) every input row is kept once.
    CallSubquery {
        scope: CallScope,
        query: Box<Query>,
        /// `IN [n] [CONCURRENT] TRANSACTIONS …`: the subquery runs in batches of input
        /// rows, each in a transaction of its own that commits before the next.
        in_transactions: Option<InTransactions>,
        /// `OPTIONAL CALL { … }`: an input row for which the subquery returns no rows is
        /// kept once, with its columns null.
        optional: bool,
    },
    /// `FOREACH (var IN list | updates)`: runs the update clauses once per list element,
    /// per row. Rows pass through unchanged.
    Foreach {
        var: String,
        list: Expr,
        body: Vec<Clause>,
    },
    /// `LOAD CSV [WITH HEADERS] FROM url AS var [FIELDTERMINATOR 'c']`: a row per CSV
    /// record, bound to `var` as a list of strings (a map by header with headers).
    LoadCsv {
        headers: bool,
        url: Expr,
        var: String,
        terminator: Option<String>,
    },
    /// `CALL name.space.proc(args) [YIELD …]`: a row per row the procedure returns, per
    /// input row. `args: None` is the implicit form (no parentheses; arguments come
    /// from the parameters of the same names), allowed only in a standalone call.
    CallProcedure {
        name: String,
        args: Option<Vec<Expr>>,
        yields: Option<ProcedureYield>,
        /// `OPTIONAL CALL`: an input row for which the procedure returns no rows is kept
        /// once, with the yielded variables null.
        optional: bool,
    },
    /// `SHOW PROCEDURES`
    ShowProcedures,
    /// `SHOW [ALL | BUILT IN | USER DEFINED] FUNCTIONS`; `user_defined`: only those (there
    /// are none).
    ShowFunctions { user_defined: bool },
    /// `CREATE [RANGE | VECTOR] INDEX [name] [IF NOT EXISTS] FOR (n:Label) ON (n.a, …)
    /// [OPTIONS {…}]`, `… FOR ()-[r:TYPE]-() ON (r.a, …)` or `CREATE INDEX ON :Label(a, …)`;
    /// `CREATE FULLTEXT INDEX [name] [IF NOT EXISTS] FOR (n:A|B) ON EACH [n.a, …]
    /// [OPTIONS {…}]` (or `FOR ()-[r:A|B]-()`).
    CreateIndex {
        name: Option<String>,
        entity: IndexEntity,
        /// The label, or the relationship type (several for a full-text index).
        labels: Vec<String>,
        properties: Vec<String>,
        if_not_exists: bool,
        kind: IndexType,
        /// The `OPTIONS` map (or parameter).
        options: Option<Expr>,
    },
    /// `DROP INDEX name [IF EXISTS]` or `DROP INDEX ON :Label(a, …)`.
    DropIndex {
        target: IndexTarget,
        if_exists: bool,
    },
    /// `SHOW INDEXES`
    ShowIndexes,
    /// `CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:Label) REQUIRE n.p IS UNIQUE`
    /// (or `(n.a, n.b) IS UNIQUE`, `n.p IS NOT NULL`, `(n.a, …) IS NODE KEY`).
    CreateConstraint {
        name: Option<String>,
        kind: ConstraintKind,
        label: String,
        properties: Vec<String>,
        if_not_exists: bool,
    },
    /// `DROP CONSTRAINT name [IF EXISTS]`
    DropConstraint { name: String, if_exists: bool },
    /// `SHOW CONSTRAINTS`
    ShowConstraints,
}

/// Which outer variables a `CALL { … }` subquery can use.
#[derive(Debug, Clone, PartialEq)]
pub enum CallScope {
    /// No scope clause: an importing `WITH a, b` (or `WITH *`) as the subquery's first
    /// clause imports those variables; without one, nothing is imported.
    Implicit,
    /// `CALL (*) { … }`: every variable in scope.
    All,
    /// `CALL (a, b) { … }` (`CALL () { … }` imports nothing).
    Vars(Vec<String>),
}

/// `YIELD *` or `YIELD field [AS var], … [WHERE pred]` after a procedure call.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcedureYield {
    /// `YIELD *`: every output, under its own name.
    pub star: bool,
    /// (output field, variable) pairs.
    pub items: Vec<(String, String)>,
    pub where_: Option<Expr>,
}

/// The kind of an expression subquery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryKind {
    /// `EXISTS { … }`: whether it yields a row.
    Exists,
    /// `COUNT { … }`: how many rows it yields.
    Count,
    /// `COLLECT { … }`: the values of its single column, as a list.
    Collect,
}

/// The kind of index `CREATE … INDEX` makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexType {
    Range,
    Vector,
    Fulltext,
}

/// How `DROP INDEX` identifies its index.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexTarget {
    Name(String),
    /// A node index, by label and properties.
    On { label: String, properties: Vec<String> },
}

impl Clause {
    pub fn is_updating(&self) -> bool {
        if let Clause::CallSubquery { query, .. } = self {
            return query.is_updating();
        }
        if let Clause::Foreach { .. } = self {
            return true;
        }
        matches!(
            self,
            Clause::Create { .. }
                | Clause::Merge { .. }
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete { .. }
                | Clause::CreateIndex { .. }
                | Clause::DropIndex { .. }
                | Clause::CreateConstraint { .. }
                | Clause::DropConstraint { .. }
        )
    }

    /// Schema commands must be the only clause in their query.
    pub fn is_schema(&self) -> bool {
        matches!(
            self,
            Clause::CreateIndex { .. }
                | Clause::DropIndex { .. }
                | Clause::ShowIndexes
                | Clause::CreateConstraint { .. }
                | Clause::DropConstraint { .. }
                | Clause::ShowConstraints
                | Clause::ShowProcedures
                | Clause::ShowFunctions { .. }
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Clause::Match { optional: true, .. } => "OPTIONAL MATCH",
            Clause::Match { .. } => "MATCH",
            Clause::Create { .. } => "CREATE",
            Clause::Merge { .. } => "MERGE",
            Clause::Set(_) => "SET",
            Clause::Remove(_) => "REMOVE",
            Clause::Delete { detach: true, .. } => "DETACH DELETE",
            Clause::Delete { .. } => "DELETE",
            Clause::Unwind { .. } => "UNWIND",
            Clause::With(_) => "WITH",
            Clause::Return(_) => "RETURN",
            Clause::CallSubquery { optional: true, .. } | Clause::CallProcedure { optional: true, .. } => "OPTIONAL CALL",
            Clause::CallSubquery { .. } => "CALL",
            Clause::Foreach { .. } => "FOREACH",
            Clause::LoadCsv { .. } => "LOAD CSV",
            Clause::CallProcedure { .. } => "CALL",
            Clause::ShowProcedures => "SHOW PROCEDURES",
            Clause::ShowFunctions { .. } => "SHOW FUNCTIONS",
            Clause::CreateIndex { .. } => "CREATE INDEX",
            Clause::DropIndex { .. } => "DROP INDEX",
            Clause::ShowIndexes => "SHOW INDEXES",
            Clause::CreateConstraint { .. } => "CREATE CONSTRAINT",
            Clause::DropConstraint { .. } => "DROP CONSTRAINT",
            Clause::ShowConstraints => "SHOW CONSTRAINTS",
        }
    }
}

/// Body of `WITH` and `RETURN`.
/// The options of `CALL { … } IN [n] [CONCURRENT] TRANSACTIONS [OF n ROWS]
/// [ON ERROR CONTINUE | BREAK | FAIL] [REPORT STATUS AS var]`.
#[derive(Debug, Clone, PartialEq)]
pub struct InTransactions {
    /// `IN n CONCURRENT TRANSACTIONS` (`Some(None)` without `n`).
    pub concurrency: Option<Option<Expr>>,
    /// `OF n ROWS`: input rows per transaction (1000 when not given).
    pub batch_size: Option<Expr>,
    pub on_error: OnError,
    /// `REPORT STATUS AS var`: a map per input row saying how its transaction went.
    pub report_status: Option<String>,
}

/// What `CALL { … } IN TRANSACTIONS` does when a batch's transaction fails.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnError {
    /// Stop with the error; batches committed before stay committed.
    #[default]
    Fail,
    /// Roll the batch back and go on with the next one.
    Continue,
    /// Roll the batch back and stop without an error.
    Break,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub distinct: bool,
    /// `RETURN *` / `WITH *`: project every variable in scope (before `items`).
    pub star: bool,
    pub items: Vec<ProjectionItem>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
    /// Only for `WITH`.
    pub where_: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionItem {
    pub expr: Expr,
    /// Explicit `AS` alias, or the expression's source text.
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortItem {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `n.key = expr`
    Property {
        var: String,
        key: String,
        value: Expr,
    },
    /// `n = {map}` — replace all properties.
    Replace { var: String, value: Expr },
    /// `n += {map}` — merge properties.
    Merge { var: String, value: Expr },
    /// `n:Label:Other`
    Labels { var: String, labels: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    Property { var: String, key: String },
    Labels { var: String, labels: Vec<String> },
}

/// `[p =] (a)-[r]->(b)...`
#[derive(Debug, Clone, PartialEq)]
pub struct PatternPart {
    pub var: Option<String>,
    pub start: NodePattern,
    pub chain: Vec<(RelPattern, NodePattern)>,
    /// `shortestPath(…)` / `allShortestPaths(…)` around the pattern (one relationship).
    pub shortest: Option<Shortest>,
}

/// Which shortest paths a `shortestPath` / `allShortestPaths` pattern finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortest {
    /// `shortestPath`: one path of the smallest length.
    One,
    /// `allShortestPaths`: every path of the smallest length.
    All,
}

impl PatternPart {
    /// All named variables introduced by the pattern, in order of appearance.
    pub fn variables(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if let Some(v) = &self.var {
            out.push(v.as_str());
        }
        if let Some(v) = &self.start.var {
            out.push(v.as_str());
        }
        for (r, n) in &self.chain {
            if let Some(v) = &r.var {
                out.push(v.as_str());
            }
            if let Some(v) = &n.var {
                out.push(v.as_str());
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub var: Option<String>,
    /// Labels the node must have (`:A:B`, `:A&B`). With [`label_expr`](Self::label_expr)
    /// set, the labels it requires in any case (the planner may scan by them).
    pub labels: Vec<String>,
    /// A label expression that is more than a conjunction of labels (`:A|B`, `:!A`,
    /// `:%`); the node must also satisfy it.
    pub label_expr: Option<LabelExpr>,
    /// A map literal or a parameter.
    pub props: Option<Expr>,
}

/// A label expression (`A|B`, `A&B`, `!A`, `%`, parentheses). On a relationship, the
/// relationship's type is its only label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelExpr {
    Label(String),
    /// `%`: any label at all.
    Any,
    Not(Box<LabelExpr>),
    And(Vec<LabelExpr>),
    Or(Vec<LabelExpr>),
}

impl LabelExpr {
    /// Whether an entity with `labels` satisfies the expression.
    pub fn matches(&self, labels: &[impl AsRef<str>]) -> bool {
        match self {
            LabelExpr::Label(l) => labels.iter().any(|x| x.as_ref() == l),
            LabelExpr::Any => !labels.is_empty(),
            LabelExpr::Not(e) => !e.matches(labels),
            LabelExpr::And(es) => es.iter().all(|e| e.matches(labels)),
            LabelExpr::Or(es) => es.iter().any(|e| e.matches(labels)),
        }
    }

    /// The labels, if the expression only requires each of them (`A`, `A&B`).
    pub fn as_conjunction(&self) -> Option<Vec<String>> {
        match self {
            LabelExpr::Label(l) => Some(vec![l.clone()]),
            LabelExpr::And(es) => {
                let mut out = Vec::new();
                for e in es {
                    out.extend(e.as_conjunction()?);
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// The types, if the expression only allows each of them (`A`, `A|B`).
    pub fn as_disjunction(&self) -> Option<Vec<String>> {
        match self {
            LabelExpr::Label(l) => Some(vec![l.clone()]),
            LabelExpr::Or(es) => {
                let mut out = Vec::new();
                for e in es {
                    out.extend(e.as_disjunction()?);
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// Labels every match has: the plain labels among the top-level conjuncts.
    pub fn required(&self) -> Vec<String> {
        match self {
            LabelExpr::Label(l) => vec![l.clone()],
            LabelExpr::And(es) => es.iter().flat_map(LabelExpr::required).collect(),
            _ => Vec::new(),
        }
    }
}

impl std::fmt::Display for LabelExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let join = |f: &mut std::fmt::Formatter<'_>, es: &[LabelExpr], sep: &str| {
            for (i, e) in es.iter().enumerate() {
                if i > 0 {
                    f.write_str(sep)?;
                }
                match e {
                    LabelExpr::And(_) | LabelExpr::Or(_) => write!(f, "({e})")?,
                    _ => write!(f, "{e}")?,
                }
            }
            Ok(())
        };
        match self {
            LabelExpr::Label(l) => f.write_str(l),
            LabelExpr::Any => f.write_str("%"),
            LabelExpr::Not(e) => match &**e {
                LabelExpr::And(_) | LabelExpr::Or(_) => write!(f, "!({e})"),
                _ => write!(f, "!{e}"),
            },
            LabelExpr::And(es) => join(f, es, "&"),
            LabelExpr::Or(es) => join(f, es, "|"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    /// `-->`
    Right,
    /// `<--`
    Left,
    /// `--`
    Both,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub var: Option<String>,
    /// Alternatives (`:A|B`); empty means any type.
    pub types: Vec<String>,
    /// A type expression that is more than a choice of types (`:!A`, `:%`, `:A&B`); the
    /// relationship must also satisfy it (`types` is then empty).
    pub type_expr: Option<LabelExpr>,
    pub direction: RelDirection,
    pub props: Option<Expr>,
    /// Variable length `*min..max`; `None` means exactly one hop.
    pub length: Option<VarLength>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarLength {
    pub min: u64,
    pub max: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
    Pos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    Xor,
    And,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    In,
    StartsWith,
    EndsWith,
    Contains,
    RegexMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantifier {
    All,
    Any,
    None,
    Single,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Param(String),
    Var(String),
    Property(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    HasLabels(Box<Expr>, Vec<String>),
    /// `x:A|B`, `x:!A`, `x:%`: a label expression more than a conjunction of labels.
    /// On a relationship it tests the type.
    HasLabelExpr(Box<Expr>, LabelExpr),
    /// `n {.name, .*, key: expr, var}`: a map built from `base` (a node, relationship or
    /// map); `null` when `base` is `null`.
    MapProjection {
        base: Box<Expr>,
        items: Vec<MapProjectionItem>,
    },
    /// Function call; `name` is lower-cased.
    Function {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
    },
    CountStar,
    Case {
        subject: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    /// `[x IN list WHERE pred | map]`
    ListComprehension {
        var: String,
        list: Box<Expr>,
        filter: Option<Box<Expr>>,
        map: Option<Box<Expr>>,
    },
    /// `reduce(acc = init, x IN list | expr)`: folds `list`, binding the running value to
    /// `acc` and each element to `var` in `expr`.
    Reduce {
        acc: String,
        init: Box<Expr>,
        var: String,
        list: Box<Expr>,
        expr: Box<Expr>,
    },
    /// `all/any/none/single(x IN list WHERE pred)`
    Quantified {
        kind: Quantifier,
        var: String,
        list: Box<Expr>,
        pred: Box<Expr>,
    },
    /// A relationship pattern used as a boolean predicate: `WHERE (a)-[:KNOWS]->(b)`.
    PatternPredicate(Box<PatternPart>),
    /// `EXISTS { … }`, `COUNT { … }`, `COLLECT { … }`. The subquery sees every variable
    /// of the outer query. `names`: every identifier written inside it (a superset of
    /// the outer variables it uses).
    Subquery {
        kind: SubqueryKind,
        query: Box<Query>,
        names: Vec<String>,
    },
    /// Engine-internal: reference to the i-th computed aggregate of the current group.
    AggregateRef(usize),
}

/// One entry of a map projection; later entries replace earlier ones of the same key.
#[derive(Debug, Clone, PartialEq)]
pub enum MapProjectionItem {
    /// `.key`: the base's property (`null` when missing).
    Property(String),
    /// `.*`: every property of the base.
    AllProperties,
    /// `key: expr`
    Entry(String, Expr),
    /// `var`: the variable under its own name.
    Var(String),
}

/// The internal function `s IS [NOT] [form] NORMALIZED` parses to, with the arguments
/// `s` and the form's name (`'NFC'` when none is given). The space keeps it out of reach
/// of a function call in a query.
pub const IS_NORMALIZED: &str = "is normalized";

/// Aggregating function names.
pub const AGGREGATE_FUNCTIONS: &[&str] = &[
    "count", "sum", "avg", "min", "max", "collect", "stdev", "stdevp", "percentilecont",
    "percentiledisc",
];

impl Expr {
    pub fn is_aggregate_call(&self) -> bool {
        match self {
            Expr::CountStar => true,
            Expr::Function { name, .. } => AGGREGATE_FUNCTIONS.contains(&name.as_str()),
            _ => false,
        }
    }

    /// Whether the expression contains an aggregate anywhere.
    pub fn contains_aggregate(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= e.is_aggregate_call() || matches!(e, Expr::AggregateRef(_)));
        found
    }

    /// Pre-order traversal over this expression and all sub-expressions.
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_)
            | Expr::Param(_)
            | Expr::Var(_)
            | Expr::CountStar
            | Expr::AggregateRef(_) => {}
            Expr::PatternPredicate(_) | Expr::Subquery { .. } => {}
            Expr::Property(e, _) | Expr::Unary(_, e) | Expr::HasLabels(e, _) | Expr::HasLabelExpr(e, _) => {
                e.walk(f)
            }
            Expr::MapProjection { base, items } => {
                base.walk(f);
                for item in items {
                    match item {
                        MapProjectionItem::Entry(_, e) => e.walk(f),
                        MapProjectionItem::Var(v) => f(&Expr::Var(v.clone())),
                        MapProjectionItem::Property(_) | MapProjectionItem::AllProperties => {}
                    }
                }
            }
            Expr::IsNull { expr, .. } => expr.walk(f),
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::Slice(a, b, c) => {
                a.walk(f);
                if let Some(b) = b {
                    b.walk(f);
                }
                if let Some(c) = c {
                    c.walk(f);
                }
            }
            Expr::List(items) => items.iter().for_each(|e| e.walk(f)),
            Expr::Map(items) => items.iter().for_each(|(_, e)| e.walk(f)),
            Expr::Function { args, .. } => args.iter().for_each(|e| e.walk(f)),
            Expr::Case {
                subject,
                whens,
                else_,
            } => {
                if let Some(s) = subject {
                    s.walk(f);
                }
                for (w, t) in whens {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(e) = else_ {
                    e.walk(f);
                }
            }
            Expr::ListComprehension {
                list, filter, map, ..
            } => {
                list.walk(f);
                if let Some(x) = filter {
                    x.walk(f);
                }
                if let Some(x) = map {
                    x.walk(f);
                }
            }
            Expr::Quantified { list, pred, .. } => {
                list.walk(f);
                pred.walk(f);
            }
            Expr::Reduce { init, list, expr, .. } => {
                init.walk(f);
                list.walk(f);
                expr.walk(f);
            }
        }
    }
}

/// A top-level statement: a graph query, or a command for the database manager.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// A graph query. `USE db <query>` runs it against `db` instead of the session's
    /// current database.
    Query {
        use_db: Option<DbName>,
        query: Query,
    },
    /// `USE db` on its own: makes `db` the session's current database.
    Use(DbName),
    /// `CREATE DATABASE`, `DROP DATABASE`, `SHOW DATABASES`.
    Database(DatabaseCommand),
    /// `CREATE USER`, `GRANT ROLE`, `SHOW USERS`, …: user accounts, which belong to
    /// `graphbuff-server` rather than to the database manager.
    User(UserCommand),
    /// `SHOW TRANSACTIONS`, `TERMINATE TRANSACTIONS`: the transactions open on
    /// `graphbuff-server`.
    Transaction(Box<TransactionCommand>),
    /// `SHOW SETTINGS`: the settings of `graphbuff-server`.
    Settings(Box<SettingsCommand>),
}

/// `SHOW SETTING[S] [name, … | $names] [tail]`.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsCommand {
    /// Setting names: string expressions, or parameters holding a string or a list of
    /// strings. Empty: every setting.
    pub names: Vec<Expr>,
    pub tail: ShowTail,
}

/// `SHOW TRANSACTION[S] [ids] [tail]` or `TERMINATE TRANSACTION[S] ids [tail]`.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionCommand {
    pub terminate: bool,
    /// Transaction ids: string expressions, or parameters holding a string or a list of
    /// strings. Empty: every transaction (`SHOW` only).
    pub ids: Vec<Expr>,
    pub tail: ShowTail,
}

impl TransactionCommand {
    pub fn name(&self) -> &'static str {
        if self.terminate { "TERMINATE TRANSACTIONS" } else { "SHOW TRANSACTIONS" }
    }
}

/// What may follow a `SHOW` command: `YIELD …` (with `ORDER BY`, `SKIP`, `LIMIT` and
/// `WHERE`), then `RETURN …`; or only `WHERE …`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShowTail {
    pub yields: Option<Projection>,
    pub where_: Option<Expr>,
    pub returns: Option<Projection>,
}

/// A database name, written literally or passed as a parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbName {
    Name(String),
    Param(String),
}

/// What `CREATE DATABASE` does when the database already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingDatabase {
    /// Plain `CREATE DATABASE`: fail.
    Fail,
    /// `CREATE DATABASE … IF NOT EXISTS`: keep the existing database, do nothing.
    Keep,
    /// `CREATE OR REPLACE DATABASE`: drop the existing database and all its data first.
    Replace,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DatabaseCommand {
    /// `CREATE [OR REPLACE] DATABASE name [IF NOT EXISTS] [OPTIONS {…}] [WAIT | NOWAIT]`
    Create {
        name: DbName,
        existing: ExistingDatabase,
        /// A map literal or a parameter.
        options: Option<Expr>,
    },
    /// `DROP DATABASE name [IF EXISTS] [DESTROY DATA] [WAIT | NOWAIT]`
    Drop { name: DbName, if_exists: bool },
    /// `SHOW DATABASES`, `SHOW DATABASE name`, `SHOW DEFAULT DATABASE`
    Show(ShowDatabases),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShowDatabases {
    All,
    Named(DbName),
    Default,
}

impl DatabaseCommand {
    pub fn name(&self) -> &'static str {
        match self {
            DatabaseCommand::Create {
                existing: ExistingDatabase::Replace,
                ..
            } => "CREATE OR REPLACE DATABASE",
            DatabaseCommand::Create { .. } => "CREATE DATABASE",
            DatabaseCommand::Drop { .. } => "DROP DATABASE",
            DatabaseCommand::Show(_) => "SHOW DATABASES",
        }
    }
}

/// A password in a user command: a string literal or a parameter.
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    Literal(String),
    Param(String),
}

impl std::fmt::Debug for Secret {
    /// Never shows a literal password, so it does not end up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Secret::Literal(_) => f.write_str("Literal(***)"),
            Secret::Param(p) => write!(f, "Param({p:?})"),
        }
    }
}

/// What `CREATE USER` does when the user already exists (as for databases).
pub type ExistingUser = ExistingDatabase;

/// A user account command. User and role names are written like database names (a
/// name, a quoted name or a parameter), so they share [`DbName`].
#[derive(Debug, Clone, PartialEq)]
pub enum UserCommand {
    /// `SHOW USERS`
    ShowUsers,
    /// `SHOW CURRENT USER`
    ShowCurrentUser,
    /// `SHOW ROLES`
    ShowRoles,
    /// `CREATE [OR REPLACE] USER name [IF NOT EXISTS] SET [PLAINTEXT] PASSWORD pw
    /// [CHANGE NOT REQUIRED] [SET STATUS ACTIVE]`
    Create {
        name: DbName,
        existing: ExistingUser,
        password: Secret,
    },
    /// `ALTER USER name [IF EXISTS] SET [PLAINTEXT] PASSWORD pw [CHANGE NOT REQUIRED]
    /// [SET STATUS ACTIVE]`; without a password the command changes nothing.
    Alter {
        name: DbName,
        if_exists: bool,
        password: Option<Secret>,
    },
    /// `ALTER CURRENT USER SET PASSWORD FROM old TO new`
    AlterCurrent { old: Secret, new: Secret },
    /// `RENAME USER from [IF EXISTS] TO to`
    Rename {
        from: DbName,
        to: DbName,
        if_exists: bool,
    },
    /// `DROP USER name [IF EXISTS]`
    Drop { name: DbName, if_exists: bool },
    /// `GRANT ROLE[S] r, … [ON DATABASE db] TO u, …`: everywhere, or (`database`) on one
    /// database only.
    Grant { roles: Vec<DbName>, database: Option<DbName>, users: Vec<DbName> },
    /// `REVOKE ROLE[S] r, … [ON DATABASE db] FROM u, …`
    Revoke { roles: Vec<DbName>, database: Option<DbName>, users: Vec<DbName> },
}

impl UserCommand {
    pub fn name(&self) -> &'static str {
        match self {
            UserCommand::ShowUsers => "SHOW USERS",
            UserCommand::ShowCurrentUser => "SHOW CURRENT USER",
            UserCommand::ShowRoles => "SHOW ROLES",
            UserCommand::Create { .. } => "CREATE USER",
            UserCommand::Alter { .. } => "ALTER USER",
            UserCommand::AlterCurrent { .. } => "ALTER CURRENT USER",
            UserCommand::Rename { .. } => "RENAME USER",
            UserCommand::Drop { .. } => "DROP USER",
            UserCommand::Grant { .. } => "GRANT ROLE",
            UserCommand::Revoke { .. } => "REVOKE ROLE",
        }
    }

    /// Whether the command changes accounts (rather than only showing them).
    pub fn is_updating(&self) -> bool {
        !matches!(
            self,
            UserCommand::ShowUsers | UserCommand::ShowCurrentUser | UserCommand::ShowRoles
        )
    }
}
