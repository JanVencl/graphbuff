//! Recursive-descent parser producing the [`crate::ast`] tree.
//!
//! Operator precedence, lowest to highest:
//! `OR` < `XOR` < `AND` < `NOT` < comparison (`= <> < > <= >= =~`) <
//! string/list/null predicates (`IN`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `IS [NOT] NULL`) <
//! `+ -` < `* / %` < `^` < unary `- +` < postfix (`.prop`, `[i]`, `[a..b]`, `:Label`) < atoms.

use graphbuff_core::{Error, Result, Value};

use crate::ast::*;
use crate::lexer::{Tok, Token, tokenize};

/// Parses a single graph query (an optional trailing `;` is allowed).
///
/// Database commands and `USE` are rejected, because they need a session that can
/// switch databases. Use [`parse_statement`] to accept them.
pub fn parse(src: &str) -> Result<Query> {
    match parse_statement(src)? {
        Statement::Query {
            use_db: None,
            query,
        } => Ok(query),
        Statement::Query { .. } | Statement::Use(_) => Err(Error::Unsupported(
            "USE needs a DatabaseManager session; a single Database has no other databases".into(),
        )),
        Statement::Database(cmd) => Err(Error::Unsupported(format!(
            "{} needs a DatabaseManager session; a single Database cannot manage databases",
            cmd.name()
        ))),
        Statement::User(cmd) => Err(Error::Unsupported(format!(
            "{} needs graphbuff-server with user accounts",
            cmd.name()
        ))),
        Statement::Transaction(cmd) => Err(Error::Unsupported(format!("{} needs graphbuff-server", cmd.name()))),
        Statement::Settings(_) => Err(Error::Unsupported("SHOW SETTINGS needs graphbuff-server".into())),
    }
}

/// Parses one statement: a graph query (optionally prefixed with `USE db`), `USE db`
/// on its own, or a database command such as `CREATE OR REPLACE DATABASE`.
pub fn parse_statement(src: &str) -> Result<Statement> {
    Parser::new(src)?.parse_statement()
}

/// Parses a standalone expression. Mostly useful for tests and tooling.
pub fn parse_expression(src: &str) -> Result<Expr> {
    let mut p = Parser::new(src)?;
    let e = p.parse_expr()?;
    if *p.peek() != Tok::Eof {
        return p.err(format!("unexpected {}", p.describe()));
    }
    Ok(e)
}

/// The Unicode normal form keyword `word` (`NFC`, `NFD`, `NFKC`, `NFKD`, any case), upper-cased.
fn normal_form(word: &str) -> Option<&'static str> {
    ["NFC", "NFD", "NFKC", "NFKD"]
        .into_iter()
        .find(|f| f.eq_ignore_ascii_case(word))
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
    /// In the `WHERE` of a list or pattern comprehension, where `|` starts the mapping
    /// expression: `x:A|B` there stops before the `|` (parenthesize to use it).
    pipe_ends_where: bool,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Result<Self> {
        Ok(Parser {
            src,
            toks: tokenize(src)?,
            pos: 0,
            pipe_ends_where: false,
        })
    }

    // ---- token helpers ----------------------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn offset(&self) -> usize {
        self.toks[self.pos].start
    }

    fn advance(&mut self) -> &Token {
        let i = self.pos;
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        &self.toks[i]
    }

    fn prev_end(&self) -> usize {
        if self.pos == 0 {
            0
        } else {
            self.toks[self.pos - 1].end
        }
    }

    fn describe(&self) -> String {
        let t = &self.toks[self.pos];
        if t.tok == Tok::Eof {
            "end of input".to_string()
        } else {
            format!("'{}'", &self.src[t.start..t.end])
        }
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T> {
        Err(Error::syntax(msg, self.offset()))
    }

    fn kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.kw_at(0, kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.err(format!("expected {kw}, found {}", self.describe()))
        }
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Tok, what: &str) -> Result<()> {
        if self.eat(&t) {
            Ok(())
        } else {
            self.err(format!("expected {what}, found {}", self.describe()))
        }
    }

    fn at_ident(&self) -> bool {
        matches!(self.peek(), Tok::Ident(_) | Tok::QuotedIdent(_))
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek().clone() {
            Tok::Ident(s) | Tok::QuotedIdent(s) => {
                self.advance();
                Ok(s)
            }
            _ => self.err(format!("expected a name, found {}", self.describe())),
        }
    }

    // ---- statements -------------------------------------------------------------------

    fn parse_statement(&mut self) -> Result<Statement> {
        let mode = if self.eat_kw("EXPLAIN") {
            QueryMode::Explain
        } else if self.eat_kw("PROFILE") {
            QueryMode::Profile
        } else {
            QueryMode::Run
        };
        let mut stmt = self.parse_plain_statement()?;
        if mode != QueryMode::Run {
            match &mut stmt {
                Statement::Query { query, .. } => query.mode = mode,
                _ => {
                    return Err(Error::Syntax {
                        message: format!(
                            "{} only applies to graph queries",
                            if mode == QueryMode::Explain { "EXPLAIN" } else { "PROFILE" }
                        ),
                        offset: 0,
                    });
                }
            }
        }
        Ok(stmt)
    }

    fn parse_plain_statement(&mut self) -> Result<Statement> {
        if (self.is_kw("SHOW") || self.is_kw("TERMINATE"))
            && (self.kw_at(1, "TRANSACTIONS") || self.kw_at(1, "TRANSACTION"))
        {
            let terminate = self.is_kw("TERMINATE");
            self.pos += 2;
            return self.parse_transaction_command(terminate);
        }
        if self.is_kw("SHOW") && (self.kw_at(1, "SETTINGS") || self.kw_at(1, "SETTING")) {
            self.pos += 2;
            let (names, tail) = self.parse_show_names_and_tail("SHOW SETTINGS")?;
            return Ok(Statement::Settings(Box::new(SettingsCommand { names, tail })));
        }
        if let Some(cmd) = self.parse_user_command()? {
            return Ok(Statement::User(cmd));
        }
        if self.is_kw("CREATE") && (self.kw_at(1, "ALIAS") || (self.kw_at(1, "OR") && self.kw_at(3, "ALIAS"))) {
            self.advance();
            return self.parse_create_alias();
        }
        if self.is_kw("CREATE") && (self.kw_at(1, "DATABASE") || self.kw_at(1, "OR")) {
            self.advance();
            return self.parse_create_database();
        }
        if self.is_kw("DROP") && self.kw_at(1, "DATABASE") {
            self.advance();
            self.advance();
            return self.parse_drop_database();
        }
        if self.is_kw("ALTER") && self.kw_at(1, "DATABASE") {
            self.pos += 2;
            return self.parse_alter_database();
        }
        if self.is_kw("ALTER") && self.kw_at(1, "ALIAS") {
            self.pos += 2;
            let alias = self.db_name()?;
            let if_exists = self.eat_kw("IF") && {
                self.expect_kw("EXISTS")?;
                true
            };
            self.expect_kw("SET")?;
            self.expect_kw("DATABASE")?;
            if !self.eat_kw("TARGET") {
                return Err(Error::Unsupported(format!(
                    "ALTER ALIAS … SET DATABASE {} is not supported: only TARGET",
                    self.describe()
                )));
            }
            let target = self.db_name()?;
            self.alias_extras()?;
            self.end_of_statement()?;
            return Ok(Statement::Database(DatabaseCommand::AlterAlias { alias, if_exists, target }));
        }
        if self.is_kw("DROP") && self.kw_at(1, "ALIAS") {
            self.pos += 2;
            let alias = self.db_name()?;
            let if_exists = self.eat_kw("IF") && {
                self.expect_kw("EXISTS")?;
                true
            };
            self.expect_kw("FOR")?;
            self.expect_kw("DATABASE")?;
            self.end_of_statement()?;
            return Ok(Statement::Database(DatabaseCommand::DropAlias { alias, if_exists }));
        }
        if self.is_kw("SHOW") && (self.kw_at(1, "ALIASES") || self.kw_at(1, "ALIAS")) {
            self.pos += 2;
            let name = if self.is_kw("FOR") { None } else { Some(self.db_name()?) };
            self.expect_kw("FOR")?;
            if !self.eat_kw("DATABASES") {
                self.expect_kw("DATABASE")?;
            }
            if self.is_kw("YIELD") || self.is_kw("WHERE") {
                return Err(Error::Unsupported("SHOW ALIASES … YIELD / WHERE is not supported".into()));
            }
            self.end_of_statement()?;
            return Ok(Statement::Database(DatabaseCommand::ShowAliases(name)));
        }
        if self.is_kw("SHOW") {
            if self.kw_at(1, "DEFAULT") && self.kw_at(2, "DATABASE") {
                self.pos += 3;
                self.end_of_statement()?;
                return Ok(Statement::Database(DatabaseCommand::Show(
                    ShowDatabases::Default,
                )));
            }
            if self.kw_at(1, "DATABASES") || self.kw_at(1, "DATABASE") {
                self.pos += 2;
                let target = if matches!(self.peek(), Tok::Eof | Tok::Semicolon) {
                    ShowDatabases::All
                } else {
                    ShowDatabases::Named(self.db_name()?)
                };
                self.end_of_statement()?;
                return Ok(Statement::Database(DatabaseCommand::Show(target)));
            }
        }
        if self.eat_kw("USE") {
            let name = self.db_name()?;
            let alone = match self.peek() {
                Tok::Eof => true,
                Tok::Semicolon => *self.peek_at(1) == Tok::Eof,
                _ => false,
            };
            if alone {
                self.end_of_statement()?;
                return Ok(Statement::Use(name));
            }
            let query = self.parse_query()?;
            return Ok(Statement::Query {
                use_db: Some(name),
                query,
            });
        }
        Ok(Statement::Query {
            use_db: None,
            query: self.parse_query()?,
        })
    }

    /// An optional `;`, then the end of the input.
    fn end_of_statement(&mut self) -> Result<()> {
        self.eat(&Tok::Semicolon);
        if *self.peek() != Tok::Eof {
            return self.err(format!("unexpected {}", self.describe()));
        }
        Ok(())
    }

    /// A database name: `name`, `name.with.dots`, `` `any-name` `` or `$param`.
    fn db_name(&mut self) -> Result<DbName> {
        if let Tok::Param(p) = self.peek().clone() {
            self.advance();
            return Ok(DbName::Param(p));
        }
        let mut name = self.ident()?;
        while *self.peek() == Tok::Dot && matches!(self.peek_at(1), Tok::Ident(_)) {
            self.advance();
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(DbName::Name(name))
    }

    /// Optional `WAIT [n [SEC | SECOND | SECONDS]]` or `NOWAIT`. Commands always finish
    /// before returning, so both are accepted and ignored.
    fn skip_wait(&mut self) {
        if self.eat_kw("NOWAIT") || !self.eat_kw("WAIT") {
            return;
        }
        if matches!(self.peek(), Tok::Int(_)) {
            self.advance();
            let _ = self.eat_kw("SEC") || self.eat_kw("SECOND") || self.eat_kw("SECONDS");
        }
    }

    /// After `CREATE`.
    fn parse_create_database(&mut self) -> Result<Statement> {
        let or_replace = if self.eat_kw("OR") {
            self.expect_kw("REPLACE")?;
            true
        } else {
            false
        };
        self.expect_kw("DATABASE")?;
        let name = self.db_name()?;
        let if_not_exists = if self.is_kw("IF") {
            let offset = self.offset();
            self.advance();
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            if or_replace {
                return Err(Error::syntax(
                    "CREATE OR REPLACE DATABASE cannot be combined with IF NOT EXISTS",
                    offset,
                ));
            }
            true
        } else {
            false
        };
        if self.is_kw("TOPOLOGY") {
            return Err(Error::Unsupported(
                "TOPOLOGY is not supported: graphbuff databases run on a single instance".into(),
            ));
        }
        let options = if self.eat_kw("OPTIONS") {
            if !matches!(self.peek(), Tok::LBrace | Tok::Param(_)) {
                return self.err(format!(
                    "expected a map or a parameter after OPTIONS, found {}",
                    self.describe()
                ));
            }
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.skip_wait();
        self.end_of_statement()?;
        let existing = if or_replace {
            ExistingDatabase::Replace
        } else if if_not_exists {
            ExistingDatabase::Keep
        } else {
            ExistingDatabase::Fail
        };
        Ok(Statement::Database(DatabaseCommand::Create {
            name,
            existing,
            options,
        }))
    }

    /// After `DROP DATABASE`.
    fn parse_drop_database(&mut self) -> Result<Statement> {
        let name = self.db_name()?;
        let if_exists = if self.eat_kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let dump = self.eat_kw("DUMP");
        if dump || self.eat_kw("DESTROY") {
            self.expect_kw("DATA")?;
        }
        self.skip_wait();
        self.end_of_statement()?;
        Ok(Statement::Database(DatabaseCommand::Drop {
            name,
            if_exists,
            dump,
        }))
    }

    /// After `ALTER DATABASE`: `name [IF EXISTS] SET ACCESS READ ONLY | READ WRITE`.
    fn parse_alter_database(&mut self) -> Result<Statement> {
        let name = self.db_name()?;
        let if_exists = self.eat_kw("IF") && {
            self.expect_kw("EXISTS")?;
            true
        };
        if !self.eat_kw("SET") || !self.eat_kw("ACCESS") {
            return Err(Error::Unsupported(format!(
                "ALTER DATABASE … {} is not supported: only SET ACCESS READ ONLY | READ WRITE",
                self.describe()
            )));
        }
        self.expect_kw("READ")?;
        let read_only = if self.eat_kw("ONLY") {
            true
        } else if self.eat_kw("WRITE") {
            false
        } else {
            return self.err(format!("expected ONLY or WRITE after READ, found {}", self.describe()));
        };
        self.skip_wait();
        self.end_of_statement()?;
        Ok(Statement::Database(DatabaseCommand::Alter { name, if_exists, read_only }))
    }

    /// After `CREATE`: `[OR REPLACE] ALIAS name [IF NOT EXISTS] FOR DATABASE target`.
    fn parse_create_alias(&mut self) -> Result<Statement> {
        let or_replace = self.eat_kw("OR") && {
            self.expect_kw("REPLACE")?;
            true
        };
        self.expect_kw("ALIAS")?;
        let alias = self.db_name()?;
        let offset = self.offset();
        let if_not_exists = self.eat_kw("IF") && {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        };
        if or_replace && if_not_exists {
            return Err(Error::syntax("CREATE OR REPLACE ALIAS cannot be combined with IF NOT EXISTS", offset));
        }
        self.expect_kw("FOR")?;
        self.expect_kw("DATABASE")?;
        let target = self.db_name()?;
        self.alias_extras()?;
        self.end_of_statement()?;
        let existing = if or_replace {
            ExistingDatabase::Replace
        } else if if_not_exists {
            ExistingDatabase::Keep
        } else {
            ExistingDatabase::Fail
        };
        Ok(Statement::Database(DatabaseCommand::CreateAlias { alias, target, existing }))
    }

    /// Fails on the parts of an alias command for remote databases (`AT 'url'`, `USER`,
    /// `PROPERTIES`, …), which a single instance has no use for.
    fn alias_extras(&mut self) -> Result<()> {
        for kw in ["AT", "USER", "PASSWORD", "DRIVER", "PROPERTIES", "DEFAULT"] {
            if self.is_kw(kw) {
                return Err(Error::Unsupported(format!(
                    "{kw} in an alias command is not supported: aliases name local databases only"
                )));
            }
        }
        Ok(())
    }

    /// After `SHOW TRANSACTIONS` / `TERMINATE TRANSACTIONS`: `[id, … | $ids]` (required to
    /// terminate), then `YIELD … [RETURN …]` or `WHERE …`.
    fn parse_transaction_command(&mut self, terminate: bool) -> Result<Statement> {
        let (ids, tail) = self.parse_show_names_and_tail("SHOW or TERMINATE TRANSACTIONS")?;
        if terminate && ids.is_empty() {
            return self.err("TERMINATE TRANSACTIONS needs the ids of the transactions to terminate");
        }
        Ok(Statement::Transaction(Box::new(TransactionCommand { terminate, ids, tail })))
    }

    /// After a `SHOW` command such as `SHOW TRANSACTIONS`: `[name, … | $names]`, then
    /// `YIELD … [RETURN …]` or `WHERE …`, and the end of the statement.
    fn parse_show_names_and_tail(&mut self, what: &str) -> Result<(Vec<Expr>, ShowTail)> {
        let mut ids = Vec::new();
        let at_tail = |p: &Self| {
            p.is_kw("YIELD") || p.is_kw("WHERE") || p.is_kw("RETURN") || matches!(p.peek(), Tok::Eof | Tok::Semicolon)
        };
        if !at_tail(self) {
            loop {
                ids.push(self.parse_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let mut tail = ShowTail::default();
        if self.eat_kw("YIELD") {
            tail.yields = Some(self.parse_projection(true)?);
            if self.eat_kw("RETURN") {
                tail.returns = Some(self.parse_projection(false)?);
            }
        } else if self.eat_kw("WHERE") {
            tail.where_ = Some(self.parse_expr()?);
        } else if self.is_kw("RETURN") {
            return self.err(format!("RETURN after {what} needs a YIELD before it"));
        }
        self.end_of_statement()?;
        Ok((ids, tail))
    }

    // ---- user commands ----

    /// A user command, if the statement starts like one.
    fn parse_user_command(&mut self) -> Result<Option<UserCommand>> {
        let cmd = if self.is_kw("SHOW") && (self.kw_at(1, "USERS") || (self.kw_at(1, "USER") && !self.kw_at(2, "DEFINED"))) {
            self.pos += 2;
            UserCommand::ShowUsers
        } else if self.is_kw("SHOW") && self.kw_at(1, "CURRENT") && self.kw_at(2, "USER") {
            self.pos += 3;
            UserCommand::ShowCurrentUser
        } else if self.is_kw("SHOW") && (self.kw_at(1, "ROLES") || self.kw_at(1, "ROLE")) {
            self.pos += 2;
            if self.is_kw("WITH") {
                return Err(Error::Unsupported(
                    "SHOW ROLES WITH USERS is not supported; SHOW USERS lists each user's roles".into(),
                ));
            }
            UserCommand::ShowRoles
        } else if self.is_kw("CREATE")
            && (self.kw_at(1, "USER") || (self.kw_at(1, "OR") && self.kw_at(2, "REPLACE") && self.kw_at(3, "USER")))
        {
            self.advance();
            let or_replace = self.eat_kw("OR");
            if or_replace {
                self.expect_kw("REPLACE")?;
            }
            self.expect_kw("USER")?;
            let name = self.db_name()?;
            let offset = self.offset();
            let if_not_exists = self.eat_kw("IF");
            if if_not_exists {
                self.expect_kw("NOT")?;
                self.expect_kw("EXISTS")?;
                if or_replace {
                    return Err(Error::syntax(
                        "CREATE OR REPLACE USER cannot be combined with IF NOT EXISTS",
                        offset,
                    ));
                }
            }
            let Some(password) = self.user_settings(true)? else {
                return self.err("CREATE USER needs SET PASSWORD");
            };
            let existing = if or_replace {
                ExistingUser::Replace
            } else if if_not_exists {
                ExistingUser::Keep
            } else {
                ExistingUser::Fail
            };
            UserCommand::Create {
                name,
                existing,
                password,
            }
        } else if self.is_kw("ALTER") && self.kw_at(1, "CURRENT") && self.kw_at(2, "USER") {
            self.pos += 3;
            self.expect_kw("SET")?;
            self.expect_kw("PASSWORD")?;
            self.expect_kw("FROM")?;
            let old = self.secret()?;
            self.expect_kw("TO")?;
            let new = self.secret()?;
            UserCommand::AlterCurrent { old, new }
        } else if self.is_kw("ALTER") && self.kw_at(1, "USER") {
            self.pos += 2;
            let name = self.db_name()?;
            let if_exists = self.eat_kw("IF");
            if if_exists {
                self.expect_kw("EXISTS")?;
            }
            if !self.is_kw("SET") && !self.is_kw("REMOVE") {
                return self.err(format!("expected SET after ALTER USER, found {}", self.describe()));
            }
            let password = self.user_settings(false)?;
            UserCommand::Alter {
                name,
                if_exists,
                password,
            }
        } else if self.is_kw("RENAME") && self.kw_at(1, "USER") {
            self.pos += 2;
            let from = self.db_name()?;
            let if_exists = self.eat_kw("IF");
            if if_exists {
                self.expect_kw("EXISTS")?;
            }
            self.expect_kw("TO")?;
            let to = self.db_name()?;
            UserCommand::Rename { from, to, if_exists }
        } else if self.is_kw("DROP") && self.kw_at(1, "USER") {
            self.pos += 2;
            let name = self.db_name()?;
            let if_exists = self.eat_kw("IF");
            if if_exists {
                self.expect_kw("EXISTS")?;
            }
            UserCommand::Drop { name, if_exists }
        } else if (self.is_kw("GRANT") || self.is_kw("REVOKE")) && (self.kw_at(1, "ROLE") || self.kw_at(1, "ROLES")) {
            let grant = self.is_kw("GRANT");
            self.pos += 2;
            let roles = self.name_list()?;
            // `ON DATABASE db`; `ON DATABASES *` is the same as no `ON`.
            let database = if self.eat_kw("ON") {
                if self.eat_kw("DATABASES") {
                    self.expect(Tok::Star, "'*'")?;
                    None
                } else {
                    self.expect_kw("DATABASE")?;
                    Some(self.db_name()?)
                }
            } else {
                None
            };
            self.expect_kw(if grant { "TO" } else { "FROM" })?;
            let users = self.name_list()?;
            if grant {
                UserCommand::Grant { roles, database, users }
            } else {
                UserCommand::Revoke { roles, database, users }
            }
        } else {
            return Ok(None);
        };
        self.end_of_statement()?;
        Ok(Some(cmd))
    }

    /// Names separated by commas.
    fn name_list(&mut self) -> Result<Vec<DbName>> {
        let mut names = vec![self.db_name()?];
        while self.eat(&Tok::Comma) {
            names.push(self.db_name()?);
        }
        Ok(names)
    }

    /// A password: a string or a parameter.
    fn secret(&mut self) -> Result<Secret> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.advance();
                Ok(Secret::Literal(s))
            }
            Tok::Param(p) => {
                self.advance();
                Ok(Secret::Param(p))
            }
            _ => self.err(format!("expected a password string or parameter, found {}", self.describe())),
        }
    }

    /// The `SET …` parts of `CREATE USER` / `ALTER USER`: returns the password, if one
    /// is set. Accepts `CHANGE NOT REQUIRED` and `SET STATUS ACTIVE`, which describe
    /// how every account here behaves; refuses what accounts cannot do.
    fn user_settings(&mut self, create: bool) -> Result<Option<Secret>> {
        let mut password = None;
        let unsupported = |what: &str| Err(Error::Unsupported(format!("{what} is not supported")));
        loop {
            if self.is_kw("REMOVE") {
                return unsupported("REMOVE HOME DATABASE / REMOVE AUTH");
            }
            if !self.eat_kw("SET") {
                break;
            }
            if self.eat_kw("PLAINTEXT") || self.is_kw("PASSWORD") {
                self.expect_kw("PASSWORD")?;
                if self.is_kw("CHANGE") {
                    // `ALTER USER x SET PASSWORD CHANGE NOT REQUIRED`
                    if create {
                        return self.err("CREATE USER needs a password");
                    }
                } else {
                    password = Some(self.secret()?);
                }
                if self.eat_kw("CHANGE") {
                    if !self.eat_kw("NOT") {
                        return unsupported("CHANGE REQUIRED (accounts never have to change their password)");
                    }
                    self.expect_kw("REQUIRED")?;
                }
            } else if self.is_kw("ENCRYPTED") {
                return unsupported("SET ENCRYPTED PASSWORD");
            } else if self.eat_kw("STATUS") {
                if self.is_kw("SUSPENDED") {
                    return unsupported("SET STATUS SUSPENDED");
                }
                self.expect_kw("ACTIVE")?;
            } else if self.is_kw("HOME") {
                return unsupported("SET HOME DATABASE");
            } else if self.is_kw("AUTH") {
                return unsupported("SET AUTH");
            } else {
                return self.err(format!("expected PASSWORD or STATUS after SET, found {}", self.describe()));
            }
        }
        Ok(password)
    }

    fn parse_query(&mut self) -> Result<Query> {
        let q = self.parse_union()?;
        self.eat(&Tok::Semicolon);
        if *self.peek() != Tok::Eof {
            return self.err(format!("unexpected {}", self.describe()));
        }
        Ok(q)
    }

    /// Single queries joined by `UNION` / `UNION ALL`.
    fn parse_union(&mut self) -> Result<Query> {
        let mut parts = vec![self.parse_single()?];
        let mut union_all: Option<bool> = None;
        while self.eat_kw("UNION") {
            let all = self.eat_kw("ALL");
            if union_all.is_some_and(|prev| prev != all) {
                return self.err("cannot mix UNION and UNION ALL");
            }
            union_all = Some(all);
            parts.push(self.parse_single()?);
        }
        Ok(Query {
            parts,
            union_all: union_all.unwrap_or(false),
            mode: QueryMode::Run,
        })
    }

    fn parse_single(&mut self) -> Result<SingleQuery> {
        let mut clauses = Vec::new();
        while !matches!(self.peek(), Tok::Eof | Tok::Semicolon | Tok::RBrace) && !self.is_kw("UNION") {
            clauses.push(self.parse_clause()?);
        }
        if clauses.is_empty() {
            return self.err(format!(
                "expected a clause such as MATCH, CREATE or RETURN, found {}",
                self.describe()
            ));
        }
        Ok(SingleQuery { clauses })
    }

    fn parse_clause(&mut self) -> Result<Clause> {
        if self.eat_kw("OPTIONAL") {
            if self.eat_kw("CALL") {
                let mut clause = if matches!(self.peek(), Tok::LBrace | Tok::LParen) {
                    self.parse_call_subquery()?
                } else {
                    self.parse_call_procedure()?
                };
                match &mut clause {
                    Clause::CallSubquery { in_transactions: Some(_), .. } => {
                        return self.err("OPTIONAL CALL cannot run IN TRANSACTIONS");
                    }
                    Clause::CallProcedure { yields: Some(ProcedureYield { where_: Some(_), .. }), .. } => {
                        return Err(Error::Unsupported(
                            "OPTIONAL CALL … YIELD … WHERE is not supported; filter in a WITH … WHERE after it".into(),
                        ));
                    }
                    Clause::CallSubquery { optional, .. } | Clause::CallProcedure { optional, .. } => *optional = true,
                    _ => unreachable!("a CALL clause"),
                }
                return Ok(clause);
            }
            self.expect_kw("MATCH")?;
            return self.parse_match(true);
        }
        if self.eat_kw("MATCH") {
            return self.parse_match(false);
        }
        if self.eat_kw("CREATE") {
            if self.is_kw("RANGE") && self.kw_at(1, "INDEX") {
                self.advance();
            }
            if self.eat_kw("INDEX") {
                return self.parse_create_index(IndexType::Range);
            }
            for (kw, kind) in [
                ("VECTOR", IndexType::Vector),
                ("FULLTEXT", IndexType::Fulltext),
                ("POINT", IndexType::Point),
            ] {
                if self.is_kw(kw) && self.kw_at(1, "INDEX") {
                    self.advance();
                    self.advance();
                    return self.parse_create_index(kind);
                }
            }
            if self.eat_kw("CONSTRAINT") {
                return self.parse_create_constraint();
            }
            if self.kw_at(1, "INDEX") {
                return Err(Error::Unsupported(
                    "non-range index types are not supported yet".into(),
                ));
            }
            return Ok(Clause::Create {
                patterns: self.parse_pattern_list()?,
            });
        }
        if self.eat_kw("MERGE") {
            let pattern = self.parse_pattern_part()?;
            let (mut on_create, mut on_match) = (Vec::new(), Vec::new());
            while self.eat_kw("ON") {
                if self.eat_kw("CREATE") {
                    self.expect_kw("SET")?;
                    on_create.extend(self.parse_set_items()?);
                } else if self.eat_kw("MATCH") {
                    self.expect_kw("SET")?;
                    on_match.extend(self.parse_set_items()?);
                } else {
                    return self.err(format!(
                        "expected CREATE or MATCH after ON, found {}",
                        self.describe()
                    ));
                }
            }
            return Ok(Clause::Merge {
                pattern,
                on_create,
                on_match,
            });
        }
        if self.eat_kw("SET") {
            return Ok(Clause::Set(self.parse_set_items()?));
        }
        if self.eat_kw("REMOVE") {
            return Ok(Clause::Remove(self.parse_remove_items()?));
        }
        if self.eat_kw("DETACH") {
            self.expect_kw("DELETE")?;
            return Ok(Clause::Delete {
                detach: true,
                exprs: self.parse_expr_list()?,
            });
        }
        if self.eat_kw("DELETE") {
            return Ok(Clause::Delete {
                detach: false,
                exprs: self.parse_expr_list()?,
            });
        }
        if self.eat_kw("UNWIND") {
            let expr = self.parse_expr()?;
            self.expect_kw("AS")?;
            let var = self.ident()?;
            return Ok(Clause::Unwind { expr, var });
        }
        if self.eat_kw("WITH") {
            return Ok(Clause::With(self.parse_projection(true)?));
        }
        if self.eat_kw("RETURN") {
            return Ok(Clause::Return(self.parse_projection(false)?));
        }
        if self.eat_kw("DROP") {
            if self.eat_kw("CONSTRAINT") {
                let name = self.ident()?;
                let if_exists = if self.eat_kw("IF") {
                    self.expect_kw("EXISTS")?;
                    true
                } else {
                    false
                };
                return Ok(Clause::DropConstraint { name, if_exists });
            }
            self.expect_kw("INDEX")?;
            return self.parse_drop_index();
        }
        if self.is_kw("SHOW") && (self.kw_at(1, "INDEXES") || self.kw_at(1, "INDEX")) {
            self.advance();
            self.advance();
            return Ok(Clause::ShowIndexes);
        }
        if self.is_kw("SHOW") && (self.kw_at(1, "CONSTRAINTS") || self.kw_at(1, "CONSTRAINT")) {
            self.advance();
            self.advance();
            return Ok(Clause::ShowConstraints);
        }
        if self.eat_kw("FOREACH") {
            self.expect(Tok::LParen, "'(' after FOREACH")?;
            let var = self.ident()?;
            self.expect_kw("IN")?;
            let list = self.parse_expr()?;
            self.expect(Tok::Pipe, "'|' before the FOREACH updates")?;
            let mut body = Vec::new();
            while *self.peek() != Tok::RParen {
                if *self.peek() == Tok::Eof {
                    return self.err("missing ')' to close FOREACH");
                }
                body.push(self.parse_clause()?);
            }
            self.advance();
            if body.is_empty() {
                return self.err("FOREACH needs at least one update clause");
            }
            return Ok(Clause::Foreach { var, list, body });
        }
        if self.is_kw("LOAD") && self.kw_at(1, "CSV") {
            self.advance();
            self.advance();
            let headers = if self.eat_kw("WITH") {
                self.expect_kw("HEADERS")?;
                true
            } else {
                false
            };
            self.expect_kw("FROM")?;
            let url = self.parse_expr()?;
            self.expect_kw("AS")?;
            let var = self.ident()?;
            let terminator = if self.eat_kw("FIELDTERMINATOR") {
                match self.peek().clone() {
                    Tok::Str(s) => {
                        self.advance();
                        Some(s)
                    }
                    _ => return self.err("FIELDTERMINATOR needs a string literal"),
                }
            } else {
                None
            };
            return Ok(Clause::LoadCsv {
                headers,
                url,
                var,
                terminator,
            });
        }
        if self.is_kw("CALL") && matches!(self.peek_at(1), Tok::LBrace | Tok::LParen) {
            self.advance();
            return self.parse_call_subquery();
        }
        if self.is_kw("CALL") && matches!(self.peek_at(1), Tok::Ident(_) | Tok::QuotedIdent(_)) {
            self.advance();
            return self.parse_call_procedure();
        }
        if self.is_kw("SHOW") && (self.kw_at(1, "PROCEDURES") || self.kw_at(1, "PROCEDURE")) {
            self.advance();
            self.advance();
            return Ok(Clause::ShowProcedures);
        }
        if self.is_kw("SHOW") {
            // SHOW [ALL | BUILT IN | USER DEFINED] FUNCTION[S]
            let (skip, user_defined) = if self.kw_at(1, "ALL") {
                (2, false)
            } else if self.kw_at(1, "BUILT") && self.kw_at(2, "IN") {
                (3, false)
            } else if self.kw_at(1, "USER") && self.kw_at(2, "DEFINED") {
                (3, true)
            } else {
                (1, false)
            };
            if self.kw_at(skip, "FUNCTIONS") || self.kw_at(skip, "FUNCTION") {
                self.pos += skip + 1;
                if self.is_kw("EXECUTABLE") {
                    return Err(Error::Unsupported(
                        "SHOW FUNCTIONS EXECUTABLE is not supported; every function may be executed".into(),
                    ));
                }
                return Ok(Clause::ShowFunctions { user_defined });
            }
        }
        for unsupported in ["CALL", "LOAD", "USE", "DROP", "SHOW"] {
            if self.is_kw(unsupported) {
                return Err(Error::Unsupported(format!(
                    "{unsupported} is not supported yet"
                )));
            }
        }
        self.err(format!(
            "expected a clause such as MATCH, CREATE or RETURN, found {}",
            self.describe()
        ))
    }

    /// After `CALL`: `name.space.proc[(args)] [YIELD * | YIELD field [AS var], … [WHERE …]]`.
    fn parse_call_procedure(&mut self) -> Result<Clause> {
        let mut name = self.ident()?;
        while self.eat(&Tok::Dot) {
            name.push('.');
            name.push_str(&self.ident()?);
        }
        let args = if self.eat(&Tok::LParen) {
            let mut args = Vec::new();
            if !self.eat(&Tok::RParen) {
                args = self.parse_expr_list()?;
                self.expect(Tok::RParen, "')' to close the procedure arguments")?;
            }
            Some(args)
        } else {
            None
        };
        let yields = if self.eat_kw("YIELD") {
            let mut y = ProcedureYield {
                star: false,
                items: Vec::new(),
                where_: None,
            };
            if self.eat(&Tok::Star) {
                y.star = true;
            } else {
                loop {
                    let field = self.ident()?;
                    let var = if self.eat_kw("AS") { self.ident()? } else { field.clone() };
                    y.items.push((field, var));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            if self.eat_kw("WHERE") {
                y.where_ = Some(self.parse_expr()?);
            }
            Some(y)
        } else {
            None
        };
        Ok(Clause::CallProcedure { name, args, yields, optional: false })
    }

    /// After `CALL`: `[(vars | *)] { query }`.
    fn parse_call_subquery(&mut self) -> Result<Clause> {
        let scope = if self.eat(&Tok::LParen) {
            if self.eat(&Tok::Star) {
                self.expect(Tok::RParen, "')'")?;
                CallScope::All
            } else {
                let mut vars = Vec::new();
                if !self.eat(&Tok::RParen) {
                    loop {
                        vars.push(self.ident()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen, "')' to close the variable scope")?;
                }
                CallScope::Vars(vars)
            }
        } else {
            CallScope::Implicit
        };
        self.expect(Tok::LBrace, "'{' to start the subquery")?;
        let query = self.parse_union()?;
        self.expect(Tok::RBrace, "'}' to close the subquery")?;
        let in_transactions = if self.eat_kw("IN") { Some(self.parse_in_transactions()?) } else { None };
        Ok(Clause::CallSubquery {
            scope,
            query: Box::new(query),
            in_transactions,
            optional: false,
        })
    }

    /// After `CALL { … } IN`: `[n] [CONCURRENT] TRANSACTIONS` and the options
    /// `OF n ROW[S]`, `ON ERROR …` and `REPORT STATUS AS var`, each at most once.
    fn parse_in_transactions(&mut self) -> Result<InTransactions> {
        let concurrency = if self.eat_kw("CONCURRENT") {
            Some(None)
        } else if self.is_kw("TRANSACTIONS") {
            None
        } else {
            let n = self.parse_expr()?;
            if !self.eat_kw("CONCURRENT") {
                return self.err(format!("expected CONCURRENT TRANSACTIONS, found {}", self.describe()));
            }
            Some(Some(n))
        };
        self.expect_kw("TRANSACTIONS")?;
        let mut options = InTransactions { concurrency, batch_size: None, on_error: OnError::Fail, report_status: None };
        let (mut of, mut on_error) = (false, false);
        loop {
            if self.is_kw("OF") {
                if of {
                    return self.err("OF … ROWS is given twice");
                }
                self.advance();
                of = true;
                options.batch_size = Some(self.parse_expr()?);
                if !(self.eat_kw("ROWS") || self.eat_kw("ROW")) {
                    return self.err(format!("expected ROWS, found {}", self.describe()));
                }
            } else if self.is_kw("ON") && self.kw_at(1, "ERROR") {
                if on_error {
                    return self.err("ON ERROR is given twice");
                }
                self.advance();
                self.advance();
                on_error = true;
                options.on_error = if self.eat_kw("CONTINUE") {
                    OnError::Continue
                } else if self.eat_kw("BREAK") {
                    OnError::Break
                } else if self.eat_kw("FAIL") {
                    OnError::Fail
                } else if self.is_kw("RETRY") {
                    return Err(Error::Unsupported("ON ERROR RETRY is not supported".into()));
                } else {
                    return self.err(format!("expected CONTINUE, BREAK or FAIL, found {}", self.describe()));
                };
            } else if self.is_kw("REPORT") {
                if options.report_status.is_some() {
                    return self.err("REPORT STATUS is given twice");
                }
                self.advance();
                self.expect_kw("STATUS")?;
                self.expect_kw("AS")?;
                options.report_status = Some(self.ident()?);
            } else {
                break;
            }
        }
        if options.report_status.is_some() && options.on_error == OnError::Fail {
            return Err(Error::Semantic(
                "REPORT STATUS needs ON ERROR CONTINUE or ON ERROR BREAK: with ON ERROR FAIL a failure ends the query".into(),
            ));
        }
        Ok(options)
    }

    /// After `EXISTS` / `COUNT` / `COLLECT`: `{ query }`, or for `EXISTS` and `COUNT`
    /// the short form `{ pattern [WHERE …] }` (a `MATCH` without the keyword).
    fn parse_subquery_expr(&mut self, kind: SubqueryKind) -> Result<Expr> {
        self.expect(Tok::LBrace, "'{'")?;
        let start = self.pos;
        const CLAUSES: &[&str] = &[
            "MATCH", "OPTIONAL", "WITH", "UNWIND", "RETURN", "CALL", "CREATE", "MERGE", "SET",
            "DELETE", "DETACH", "REMOVE", "FOREACH", "LOAD",
        ];
        let query = if CLAUSES.iter().any(|k| self.is_kw(k)) {
            self.parse_union()?
        } else if kind == SubqueryKind::Collect {
            return self.err("COLLECT { … } needs a full subquery ending in RETURN");
        } else {
            let patterns = self.parse_pattern_list()?;
            let where_ = if self.eat_kw("WHERE") {
                Some(self.parse_expr()?)
            } else {
                None
            };
            Query {
                parts: vec![SingleQuery {
                    clauses: vec![Clause::Match {
                        optional: false,
                        patterns,
                        where_,
                    }],
                }],
                union_all: false,
                mode: QueryMode::Run,
            }
        };
        let mut names: Vec<String> = Vec::new();
        for t in &self.toks[start..self.pos] {
            if let Tok::Ident(n) | Tok::QuotedIdent(n) = &t.tok
                && !names.contains(n)
            {
                names.push(n.clone());
            }
        }
        self.expect(Tok::RBrace, "'}' to close the subquery")?;
        Ok(Expr::Subquery {
            kind,
            query: Box::new(query),
            names,
        })
    }

    /// After `CREATE INDEX`, `CREATE VECTOR INDEX`, `CREATE FULLTEXT INDEX` or `CREATE
    /// POINT INDEX` (`kind`).
    fn parse_create_index(&mut self, kind: IndexType) -> Result<Clause> {
        let fulltext = kind == IndexType::Fulltext;
        if kind == IndexType::Range && self.eat_kw("ON") {
            let (label, properties) = self.parse_legacy_index_target()?;
            return Ok(Clause::CreateIndex {
                name: None,
                entity: IndexEntity::Node,
                labels: vec![label],
                properties,
                if_not_exists: false,
                kind,
                options: None,
            });
        }
        let name = if self.at_ident() && !self.is_kw("IF") && !self.is_kw("FOR") {
            Some(self.ident()?)
        } else {
            None
        };
        let if_not_exists = if self.eat_kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        self.expect_kw("FOR")?;
        self.expect(Tok::LParen, "'('")?;
        let (entity, var, labels) = if self.eat(&Tok::RParen) {
            // `()-[r:TYPE]-()`, in any direction.
            let offset = self.offset();
            let rel = self.parse_rel_pattern()?;
            self.expect(Tok::LParen, "'('")?;
            self.expect(Tok::RParen, "')'")?;
            let types_ok = if fulltext { !rel.types.is_empty() } else { rel.types.len() == 1 };
            let (Some(var), true) = (rel.var, types_ok) else {
                return Err(Error::syntax(
                    if fulltext {
                        "a full-text relationship index needs a variable and types, as in ()-[r:A|B]-()"
                    } else {
                        "a relationship index needs a variable and one type, as in ()-[r:TYPE]-()"
                    },
                    offset,
                ));
            };
            if rel.props.is_some() || rel.length.is_some() {
                return Err(Error::syntax(
                    "a relationship index pattern takes no properties or length",
                    offset,
                ));
            }
            (IndexEntity::Relationship, var, rel.types)
        } else {
            let var = self.ident()?;
            self.expect(Tok::Colon, "':' and a label")?;
            let mut labels = vec![self.ident()?];
            while fulltext && self.eat(&Tok::Pipe) {
                labels.push(self.ident()?);
            }
            self.expect(Tok::RParen, "')'")?;
            (IndexEntity::Node, var, labels)
        };
        self.expect_kw("ON")?;
        // A full-text index lists its properties as `EACH [n.a, …]`; a vector index is on
        // one property, with or without parentheses.
        let (open, close) = if fulltext {
            self.expect_kw("EACH")?;
            (Some(Tok::LBracket), Tok::RBracket)
        } else if kind == IndexType::Range || matches!(self.peek(), Tok::LParen) {
            (Some(Tok::LParen), Tok::RParen)
        } else {
            (None, Tok::RParen)
        };
        let parens = open.is_some();
        if let Some(open) = open {
            let what = if fulltext { "'['" } else { "'('" };
            self.expect(open, what)?;
        }
        let mut properties = Vec::new();
        loop {
            let offset = self.offset();
            let owner = self.ident()?;
            if owner != var {
                return Err(Error::syntax(
                    format!("expected `{var}.<property>`, found `{owner}`"),
                    offset,
                ));
            }
            self.expect(Tok::Dot, "'.'")?;
            properties.push(self.ident()?);
            if !parens || !self.eat(&Tok::Comma) {
                break;
            }
        }
        if parens {
            let what = if fulltext { "']'" } else { "')'" };
            self.expect(close, what)?;
        }
        let options = if self.eat_kw("OPTIONS") {
            if !matches!(self.peek(), Tok::LBrace | Tok::Param(_)) {
                return self.err(format!(
                    "expected a map or a parameter after OPTIONS, found {}",
                    self.describe()
                ));
            }
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Clause::CreateIndex {
            name,
            entity,
            labels,
            properties,
            if_not_exists,
            kind,
            options,
        })
    }

    /// After `CREATE CONSTRAINT`: `[name] [IF NOT EXISTS] FOR <target> REQUIRE <props> IS
    /// <kind>`, where `<target>` is `(n:Label)` or `()-[r:TYPE]-()`, `<props>` is `n.p` or
    /// `(n.p, n.q, …)` and `<kind>` is `UNIQUE`, `NOT NULL`, `NODE KEY`,
    /// `RELATIONSHIP KEY` or `:: <type>` (`TYPED <type>`). The older `ON … ASSERT …`
    /// spelling is accepted for nodes.
    fn parse_create_constraint(&mut self) -> Result<Clause> {
        let name = if self.at_ident() && !self.is_kw("IF") && !self.is_kw("FOR") && !self.is_kw("ON") {
            Some(self.ident()?)
        } else {
            None
        };
        let if_not_exists = if self.eat_kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        if !self.eat_kw("FOR") {
            self.expect_kw("ON")?;
        }
        self.expect(Tok::LParen, "'('")?;
        let (entity, var, label) = if self.eat(&Tok::RParen) {
            // `()-[r:TYPE]-()`, in any direction.
            let offset = self.offset();
            let rel = self.parse_rel_pattern()?;
            self.expect(Tok::LParen, "'('")?;
            self.expect(Tok::RParen, "')'")?;
            match (rel.var, rel.types.len(), &rel.props, &rel.length) {
                (Some(var), 1, None, None) => (IndexEntity::Relationship, var, rel.types[0].clone()),
                _ => {
                    return Err(Error::syntax(
                        "a relationship constraint needs a variable and one type, as in ()-[r:TYPE]-()",
                        offset,
                    ));
                }
            }
        } else {
            let var = self.ident()?;
            self.expect(Tok::Colon, "':' and a label")?;
            let label = self.ident()?;
            self.expect(Tok::RParen, "')'")?;
            (IndexEntity::Node, var, label)
        };
        if !self.eat_kw("REQUIRE") {
            self.expect_kw("ASSERT")?;
        }
        let mut properties = Vec::new();
        let list = self.eat(&Tok::LParen);
        loop {
            let offset = self.offset();
            let owner = self.ident()?;
            if owner != var {
                return Err(Error::syntax(
                    format!("expected `{var}.<property>`, found `{owner}`"),
                    offset,
                ));
            }
            self.expect(Tok::Dot, "'.'")?;
            properties.push(self.ident()?);
            if !list || !self.eat(&Tok::Comma) {
                break;
            }
        }
        if list {
            self.expect(Tok::RParen, "')'")?;
        }
        let colons = |p: &mut Self| {
            let found = *p.peek() == Tok::Colon && *p.peek_at(1) == Tok::Colon;
            if found {
                p.advance();
                p.advance();
            }
            found
        };
        // `n.p :: T`, `n.p IS :: T` or `n.p IS TYPED T`
        let typed = colons(self) || {
            self.expect_kw("IS")?;
            self.eat_kw("TYPED") || colons(self)
        };
        let offset = self.offset();
        let kind = if typed {
            ConstraintKind::Typed(self.parse_property_type()?)
        } else if self.is_kw("NODE") || self.is_kw("RELATIONSHIP") || self.is_kw("REL") {
            // `NODE` / `RELATIONSHIP` (`REL`) before UNIQUE or KEY: must match the target.
            let word = if self.eat_kw("NODE") {
                IndexEntity::Node
            } else {
                self.advance();
                IndexEntity::Relationship
            };
            if word != entity {
                return Err(Error::syntax(
                    format!(
                        "a {} constraint on a {}",
                        word.text().to_lowercase(),
                        entity.text().to_lowercase()
                    ),
                    offset,
                ));
            }
            if self.eat_kw("UNIQUE") {
                ConstraintKind::Unique
            } else {
                self.expect_kw("KEY")?;
                ConstraintKind::Key
            }
        } else if self.eat_kw("UNIQUE") {
            ConstraintKind::Unique
        } else if self.eat_kw("NOT") {
            self.expect_kw("NULL")?;
            ConstraintKind::NotNull
        } else {
            return self.err(format!(
                "expected UNIQUE, NOT NULL, {} or :: <type>, found {}",
                ConstraintKind::Key.text(entity),
                self.describe()
            ));
        };
        Ok(Clause::CreateConstraint {
            name,
            entity,
            kind,
            label,
            properties,
            if_not_exists,
        })
    }

    /// A property type in a constraint: `INTEGER`, `LIST<STRING NOT NULL>`, a union
    /// `INTEGER | FLOAT` or `ANY<INTEGER | FLOAT>`.
    fn parse_property_type(&mut self) -> Result<PropertyType> {
        let any = self.is_kw("ANY") && *self.peek_at(1) == Tok::Lt;
        if any {
            self.advance();
            self.advance();
        }
        let mut t = self.parse_type_part()?;
        while self.eat(&Tok::Pipe) {
            t = t.union(self.parse_type_part()?);
        }
        if any {
            self.expect(Tok::Gt, "'>'")?;
        }
        if self.is_kw("NOT") {
            return self.err("a property type constraint takes no NOT NULL; add a separate IS NOT NULL constraint");
        }
        Ok(t)
    }

    /// One member of a property type union: a scalar type or `LIST<scalar NOT NULL>`.
    fn parse_type_part(&mut self) -> Result<PropertyType> {
        if (self.is_kw("LIST") || self.is_kw("ARRAY")) && *self.peek_at(1) == Tok::Lt {
            self.advance();
            self.advance();
            let name = self.parse_scalar_type()?;
            if !self.eat_kw("NOT") {
                return self.err("a list type in a constraint needs NOT NULL elements, as in LIST<INTEGER NOT NULL>");
            }
            self.expect_kw("NULL")?;
            self.expect(Tok::Gt, "'>'")?;
            return Ok(PropertyType::scalar(name, true).expect("a known scalar"));
        }
        let name = self.parse_scalar_type()?;
        Ok(PropertyType::scalar(name, false).expect("a known scalar"))
    }

    /// A scalar property type, by its name in [`PropertyType::SCALARS`].
    fn parse_scalar_type(&mut self) -> Result<&'static str> {
        let zone = |p: &mut Self| -> Result<bool> {
            // `WITH TIME ZONE` / `WITHOUT TIME ZONE`
            let with = if p.eat_kw("WITH") {
                true
            } else {
                p.expect_kw("WITHOUT")?;
                false
            };
            p.expect_kw("TIME")?;
            p.expect_kw("ZONE")?;
            Ok(with)
        };
        let simple = [
            ("BOOL", "BOOLEAN"),
            ("BOOLEAN", "BOOLEAN"),
            ("STRING", "STRING"),
            ("VARCHAR", "STRING"),
            ("INT", "INTEGER"),
            ("INTEGER", "INTEGER"),
            ("INT64", "INTEGER"),
            ("INTEGER64", "INTEGER"),
            ("FLOAT", "FLOAT"),
            ("FLOAT64", "FLOAT"),
            ("DATE", "DATE"),
            ("DURATION", "DURATION"),
            ("POINT", "POINT"),
        ];
        for (kw, name) in simple {
            if self.eat_kw(kw) {
                return Ok(name);
            }
        }
        if self.eat_kw("SIGNED") {
            self.expect_kw("INTEGER")?;
            return Ok("INTEGER");
        }
        for (kw, local, zoned) in [("LOCAL", "LOCAL TIME", "LOCAL DATETIME"), ("ZONED", "ZONED TIME", "ZONED DATETIME")] {
            if self.eat_kw(kw) {
                if self.eat_kw("TIME") {
                    return Ok(local);
                }
                self.expect_kw("DATETIME")?;
                return Ok(zoned);
            }
        }
        if self.eat_kw("TIME") {
            return Ok(if zone(self)? { "ZONED TIME" } else { "LOCAL TIME" });
        }
        if self.eat_kw("TIMESTAMP") {
            return Ok(if zone(self)? { "ZONED DATETIME" } else { "LOCAL DATETIME" });
        }
        self.err(format!("expected a property type, found {}", self.describe()))
    }

    /// After `DROP INDEX`.
    fn parse_drop_index(&mut self) -> Result<Clause> {
        let target = if self.eat_kw("ON") {
            let (label, properties) = self.parse_legacy_index_target()?;
            IndexTarget::On { label, properties }
        } else {
            IndexTarget::Name(self.ident()?)
        };
        let if_exists = if self.eat_kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        Ok(Clause::DropIndex { target, if_exists })
    }

    /// `:Label(prop, …)`
    fn parse_legacy_index_target(&mut self) -> Result<(String, Vec<String>)> {
        self.expect(Tok::Colon, "':' and a label")?;
        let label = self.ident()?;
        self.expect(Tok::LParen, "'('")?;
        let mut properties = vec![self.ident()?];
        while self.eat(&Tok::Comma) {
            properties.push(self.ident()?);
        }
        self.expect(Tok::RParen, "')'")?;
        Ok((label, properties))
    }

    fn parse_match(&mut self, optional: bool) -> Result<Clause> {
        let patterns = self.parse_pattern_list()?;
        let where_ = if self.eat_kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Clause::Match {
            optional,
            patterns,
            where_,
        })
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut out = vec![self.parse_expr()?];
        while self.eat(&Tok::Comma) {
            out.push(self.parse_expr()?);
        }
        Ok(out)
    }

    fn parse_set_items(&mut self) -> Result<Vec<SetItem>> {
        let mut items = Vec::new();
        loop {
            // `SET (n).name = …`: a parenthesized variable before a property.
            let parenthesized = self.eat(&Tok::LParen);
            let var = self.ident()?;
            if parenthesized {
                self.expect(Tok::RParen, "')'")?;
                if *self.peek() != Tok::Dot {
                    return self.err("expected '.' and a property name after the parenthesized variable");
                }
            }
            if self.eat(&Tok::Dot) {
                let key = self.ident()?;
                self.expect(Tok::Eq, "'='")?;
                items.push(SetItem::Property {
                    var,
                    key,
                    value: self.parse_expr()?,
                });
            } else if self.eat(&Tok::Eq) {
                items.push(SetItem::Replace {
                    var,
                    value: self.parse_expr()?,
                });
            } else if self.eat(&Tok::PlusEq) {
                items.push(SetItem::Merge {
                    var,
                    value: self.parse_expr()?,
                });
            } else if *self.peek() == Tok::Colon {
                items.push(SetItem::Labels {
                    var,
                    labels: self.parse_label_list()?,
                });
            } else {
                return self.err(format!(
                    "expected '.', '=', '+=' or ':' in SET, found {}",
                    self.describe()
                ));
            }
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    fn parse_remove_items(&mut self) -> Result<Vec<RemoveItem>> {
        let mut items = Vec::new();
        loop {
            let var = self.ident()?;
            if self.eat(&Tok::Dot) {
                items.push(RemoveItem::Property {
                    var,
                    key: self.ident()?,
                });
            } else if *self.peek() == Tok::Colon {
                items.push(RemoveItem::Labels {
                    var,
                    labels: self.parse_label_list()?,
                });
            } else {
                return self.err(format!(
                    "expected '.' or ':' in REMOVE, found {}",
                    self.describe()
                ));
            }
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    /// Label expressions after `:`, as in `(n:A|B)` or `n:A:B`; `None` without a `:`.
    /// Several (`:A:B`) must all hold. `pipe`: whether `|` may continue the expression.
    fn parse_label_exprs(&mut self, pipe: bool) -> Result<Option<LabelExpr>> {
        let mut all = Vec::new();
        while self.eat(&Tok::Colon) {
            all.push(self.parse_label_or(pipe)?);
        }
        Ok(match all.len() {
            0 => None,
            1 => all.pop(),
            _ => Some(LabelExpr::And(all)),
        })
    }

    fn parse_label_or(&mut self, pipe: bool) -> Result<LabelExpr> {
        let mut alts = vec![self.parse_label_and()?];
        while pipe && self.eat(&Tok::Pipe) {
            self.eat(&Tok::Colon); // the old form `:A|:B`
            alts.push(self.parse_label_and()?);
        }
        Ok(if alts.len() == 1 { alts.pop().unwrap() } else { LabelExpr::Or(alts) })
    }

    fn parse_label_and(&mut self) -> Result<LabelExpr> {
        let mut all = vec![self.parse_label_not()?];
        while self.eat(&Tok::Amp) {
            all.push(self.parse_label_not()?);
        }
        Ok(if all.len() == 1 { all.pop().unwrap() } else { LabelExpr::And(all) })
    }

    fn parse_label_not(&mut self) -> Result<LabelExpr> {
        if self.eat(&Tok::Bang) {
            return Ok(LabelExpr::Not(Box::new(self.parse_label_not()?)));
        }
        if self.eat(&Tok::Percent) {
            return Ok(LabelExpr::Any);
        }
        if self.eat(&Tok::LParen) {
            let e = self.parse_label_or(true)?;
            self.expect(Tok::RParen, "')' to close the label expression")?;
            return Ok(e);
        }
        Ok(LabelExpr::Label(self.ident()?))
    }

    fn parse_label_list(&mut self) -> Result<Vec<String>> {
        let mut labels = Vec::new();
        while self.eat(&Tok::Colon) {
            labels.push(self.ident()?);
        }
        Ok(labels)
    }

    fn parse_projection(&mut self, is_with: bool) -> Result<Projection> {
        let distinct = self.eat_kw("DISTINCT");
        let mut star = false;
        let mut items = Vec::new();
        if self.eat(&Tok::Star) {
            star = true;
            if self.eat(&Tok::Comma) {
                items = self.parse_projection_items(is_with)?;
            }
        } else {
            items = self.parse_projection_items(is_with)?;
        }

        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let expr = self.parse_expr()?;
                let descending = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    true
                } else {
                    let _ = self.eat_kw("ASC") || self.eat_kw("ASCENDING");
                    false
                };
                order_by.push(SortItem { expr, descending });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if self.eat_kw("LIMIT") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let where_ = if is_with && self.eat_kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Projection {
            distinct,
            star,
            items,
            order_by,
            skip,
            limit,
            where_,
        })
    }

    /// `is_with`: every item other than a plain variable needs `AS`.
    fn parse_projection_items(&mut self, is_with: bool) -> Result<Vec<ProjectionItem>> {
        let mut items = Vec::new();
        loop {
            let start = self.offset();
            let expr = self.parse_expr()?;
            let end = self.prev_end();
            let alias = if self.eat_kw("AS") {
                self.ident()?
            } else if let Expr::Var(name) = &expr {
                name.clone()
            } else if is_with {
                return self.err(format!(
                    "expression in WITH must be aliased (use AS): {}",
                    self.src[start..end].trim()
                ));
            } else {
                self.src[start..end].trim().to_string()
            };
            items.push(ProjectionItem { expr, alias });
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    // ---- patterns ---------------------------------------------------------------------

    fn parse_pattern_list(&mut self) -> Result<Vec<PatternPart>> {
        let mut out = vec![self.parse_pattern_part()?];
        while self.eat(&Tok::Comma) {
            out.push(self.parse_pattern_part()?);
        }
        Ok(out)
    }

    fn parse_pattern_part(&mut self) -> Result<PatternPart> {
        let var = if self.at_ident() && *self.peek_at(1) == Tok::Eq {
            let v = self.ident()?;
            self.advance();
            Some(v)
        } else {
            None
        };
        let shortest = if *self.peek_at(1) == Tok::LParen && self.is_kw("shortestPath") {
            Some(Shortest::One)
        } else if *self.peek_at(1) == Tok::LParen && self.is_kw("allShortestPaths") {
            Some(Shortest::All)
        } else {
            None
        };
        if shortest.is_some() {
            self.advance();
            self.advance();
        }
        let start = self.parse_node_pattern()?;
        let mut chain = Vec::new();
        while *self.peek() == Tok::Minus
            || (*self.peek() == Tok::Lt && *self.peek_at(1) == Tok::Minus)
        {
            let rel = self.parse_rel_pattern()?;
            let node = self.parse_node_pattern()?;
            chain.push((rel, node));
        }
        if shortest.is_some() {
            self.expect(Tok::RParen, "')' to close shortestPath(...)")?;
            if chain.len() != 1 {
                return self.err("shortestPath(...) needs a pattern with exactly one relationship");
            }
            if chain[0].0.length.is_some_and(|l| l.min > 1) {
                return self.err("shortestPath(...) only supports a minimum length of 0 or 1");
            }
        }
        Ok(PatternPart {
            var,
            start,
            chain,
            shortest,
        })
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern> {
        self.expect(Tok::LParen, "'(' to start a node pattern")?;
        let var = if self.at_ident() {
            Some(self.ident()?)
        } else {
            None
        };
        let (labels, label_expr) = match self.parse_label_exprs(true)? {
            None => (Vec::new(), None),
            Some(e) => match e.as_conjunction() {
                Some(labels) => (labels, None),
                None => (e.required(), Some(e)),
            },
        };
        let props = self.parse_pattern_props()?;
        self.expect(Tok::RParen, "')' to close the node pattern")?;
        Ok(NodePattern {
            var,
            labels,
            label_expr,
            props,
        })
    }

    fn parse_pattern_props(&mut self) -> Result<Option<Expr>> {
        match self.peek().clone() {
            Tok::LBrace => Ok(Some(self.parse_map_literal()?)),
            Tok::Param(p) => {
                self.advance();
                Ok(Some(Expr::Param(p)))
            }
            _ => Ok(None),
        }
    }

    fn parse_rel_pattern(&mut self) -> Result<RelPattern> {
        let left = self.eat(&Tok::Lt);
        self.expect(Tok::Minus, "'-'")?;
        let (mut var, mut types, mut type_expr, mut props, mut length) = (None, Vec::new(), None, None, None);
        if self.eat(&Tok::LBracket) {
            if self.at_ident() {
                var = Some(self.ident()?);
            }
            if let Some(e) = self.parse_label_exprs(true)? {
                match e.as_disjunction() {
                    Some(t) => types = t,
                    None => type_expr = Some(e),
                }
            }
            if self.eat(&Tok::Star) {
                let min = self.eat_uint();
                length = Some(if self.eat(&Tok::DotDot) {
                    VarLength {
                        min: min.unwrap_or(1),
                        max: self.eat_uint(),
                    }
                } else {
                    match min {
                        Some(n) => VarLength {
                            min: n,
                            max: Some(n),
                        },
                        None => VarLength { min: 1, max: None },
                    }
                });
            }
            props = self.parse_pattern_props()?;
            self.expect(Tok::RBracket, "']' to close the relationship pattern")?;
        }
        self.expect(Tok::Minus, "'-'")?;
        let right = self.eat(&Tok::Gt);
        let direction = match (left, right) {
            (true, false) => RelDirection::Left,
            (false, true) => RelDirection::Right,
            // `<-->` points both ways, which openCypher reads as either direction.
            (false, false) | (true, true) => RelDirection::Both,
        };
        Ok(RelPattern {
            var,
            types,
            type_expr,
            direction,
            props,
            length,
        })
    }

    fn eat_uint(&mut self) -> Option<u64> {
        if let Tok::Int(n) = self.peek().clone() {
            if n < 0 {
                return None;
            }
            let n = n as u64;
            self.advance();
            Some(n)
        } else {
            None
        }
    }

    // ---- expressions ------------------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut l = self.parse_xor()?;
        while self.eat_kw("OR") {
            l = Expr::Binary(BinaryOp::Or, Box::new(l), Box::new(self.parse_xor()?));
        }
        Ok(l)
    }

    fn parse_xor(&mut self) -> Result<Expr> {
        let mut l = self.parse_and()?;
        while self.eat_kw("XOR") {
            l = Expr::Binary(BinaryOp::Xor, Box::new(l), Box::new(self.parse_and()?));
        }
        Ok(l)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut l = self.parse_not()?;
        while self.eat_kw("AND") {
            l = Expr::Binary(BinaryOp::And, Box::new(l), Box::new(self.parse_not()?));
        }
        Ok(l)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Unary(UnaryOp::Not, Box::new(self.parse_not()?)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let first = self.parse_predicate()?;
        let mut pairs: Vec<(BinaryOp, Expr)> = Vec::new();
        loop {
            let op = match self.peek() {
                Tok::Eq => BinaryOp::Eq,
                Tok::Neq => BinaryOp::Neq,
                Tok::Lt => BinaryOp::Lt,
                Tok::Le => BinaryOp::Le,
                Tok::Gt => BinaryOp::Gt,
                Tok::Ge => BinaryOp::Ge,
                Tok::RegexMatch => BinaryOp::RegexMatch,
                _ => break,
            };
            self.advance();
            pairs.push((op, self.parse_predicate()?));
        }
        // `a < b < c` means `a < b AND b < c`.
        let mut result: Option<Expr> = None;
        let mut left = first;
        for (op, right) in pairs {
            let cmp = Expr::Binary(op, Box::new(left), Box::new(right.clone()));
            result = Some(match result {
                None => cmp,
                Some(acc) => Expr::Binary(BinaryOp::And, Box::new(acc), Box::new(cmp)),
            });
            left = right;
        }
        Ok(result.unwrap_or(left))
    }

    fn parse_predicate(&mut self) -> Result<Expr> {
        let mut e = self.parse_additive()?;
        loop {
            if self.eat_kw("IS") {
                let negated = self.eat_kw("NOT");
                // `s IS [NOT] [NFC | NFD | NFKC | NFKD] NORMALIZED`
                let form = match self.peek() {
                    Tok::Ident(w) if self.kw_at(1, "NORMALIZED") => normal_form(w),
                    _ => None,
                };
                if form.is_some() {
                    self.advance();
                }
                if form.is_some() || self.is_kw("NORMALIZED") {
                    self.advance();
                    let form = Expr::Literal(Value::String(form.unwrap_or("NFC").into()));
                    let test = Expr::Function {
                        name: IS_NORMALIZED.into(),
                        distinct: false,
                        args: vec![e, form],
                    };
                    e = if negated { Expr::Unary(UnaryOp::Not, Box::new(test)) } else { test };
                    continue;
                }
                self.expect_kw("NULL")?;
                e = Expr::IsNull {
                    expr: Box::new(e),
                    negated,
                };
            } else if self.eat_kw("IN") {
                e = Expr::Binary(BinaryOp::In, Box::new(e), Box::new(self.parse_additive()?));
            } else if self.is_kw("STARTS") && self.kw_at(1, "WITH") {
                self.advance();
                self.advance();
                e = Expr::Binary(
                    BinaryOp::StartsWith,
                    Box::new(e),
                    Box::new(self.parse_additive()?),
                );
            } else if self.is_kw("ENDS") && self.kw_at(1, "WITH") {
                self.advance();
                self.advance();
                e = Expr::Binary(
                    BinaryOp::EndsWith,
                    Box::new(e),
                    Box::new(self.parse_additive()?),
                );
            } else if self.eat_kw("CONTAINS") {
                e = Expr::Binary(
                    BinaryOp::Contains,
                    Box::new(e),
                    Box::new(self.parse_additive()?),
                );
            } else {
                return Ok(e);
            }
        }
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut l = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinaryOp::Add,
                Tok::Minus => BinaryOp::Sub,
                _ => return Ok(l),
            };
            self.advance();
            l = Expr::Binary(op, Box::new(l), Box::new(self.parse_multiplicative()?));
        }
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut l = self.parse_power()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinaryOp::Mul,
                Tok::Slash => BinaryOp::Div,
                Tok::Percent => BinaryOp::Mod,
                _ => return Ok(l),
            };
            self.advance();
            l = Expr::Binary(op, Box::new(l), Box::new(self.parse_power()?));
        }
    }

    fn parse_power(&mut self) -> Result<Expr> {
        let mut l = self.parse_unary()?;
        while self.eat(&Tok::Caret) {
            l = Expr::Binary(BinaryOp::Pow, Box::new(l), Box::new(self.parse_unary()?));
        }
        Ok(l)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Minus) {
            if *self.peek() == Tok::Int(i64::MIN) {
                self.advance();
                return Ok(Expr::Literal(Value::Int(i64::MIN)));
            }
            return Ok(match self.parse_unary()? {
                Expr::Literal(Value::Int(n)) if n != i64::MIN => Expr::Literal(Value::Int(-n)),
                Expr::Literal(Value::Float(f)) => Expr::Literal(Value::Float(-f)),
                e => Expr::Unary(UnaryOp::Neg, Box::new(e)),
            });
        }
        if self.eat(&Tok::Plus) {
            return Ok(Expr::Unary(UnaryOp::Pos, Box::new(self.parse_unary()?)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut e = self.parse_atom()?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.advance();
                    e = Expr::Property(Box::new(e), self.ident()?);
                }
                Tok::LBracket => {
                    self.advance();
                    if self.eat(&Tok::DotDot) {
                        let to = self.parse_optional_slice_end()?;
                        e = Expr::Slice(Box::new(e), None, to);
                    } else {
                        let a = self.parse_expr()?;
                        if self.eat(&Tok::DotDot) {
                            let to = self.parse_optional_slice_end()?;
                            e = Expr::Slice(Box::new(e), Some(Box::new(a)), to);
                        } else {
                            self.expect(Tok::RBracket, "']'")?;
                            e = Expr::Index(Box::new(e), Box::new(a));
                        }
                    }
                }
                Tok::Colon => {
                    let pipe = !self.pipe_ends_where;
                    let expr = self.parse_label_exprs(pipe)?.expect("starts with ':'");
                    e = match expr.as_conjunction() {
                        Some(labels) => Expr::HasLabels(Box::new(e), labels),
                        None => Expr::HasLabelExpr(Box::new(e), expr),
                    };
                }
                _ => return Ok(e),
            }
        }
    }

    fn parse_optional_slice_end(&mut self) -> Result<Option<Box<Expr>>> {
        if self.eat(&Tok::RBracket) {
            return Ok(None);
        }
        let e = self.parse_expr()?;
        self.expect(Tok::RBracket, "']'")?;
        Ok(Some(Box::new(e)))
    }

    fn parse_atom(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Int(i64::MIN) => self.err("integer literal '9223372036854775808' is too large"),
            Tok::Int(n) => {
                self.advance();
                Ok(Expr::Literal(Value::Int(n)))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::String(s)))
            }
            Tok::Param(p) => {
                self.advance();
                Ok(Expr::Param(p))
            }
            Tok::LBracket => self.parse_list(),
            Tok::LBrace => self.parse_map_literal(),
            Tok::LParen => {
                // Either a pattern predicate `(a)-->(b)` or a parenthesized expression.
                let save = self.pos;
                if let Ok(part) = self.parse_pattern_part()
                    && !part.chain.is_empty()
                {
                    return Ok(Expr::PatternPredicate(Box::new(part)));
                }
                self.pos = save;
                self.advance();
                let outer = std::mem::replace(&mut self.pipe_ends_where, false);
                let e = self.parse_expr();
                self.pipe_ends_where = outer;
                let e = e?;
                self.expect(Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::QuotedIdent(name) => {
                self.advance();
                Ok(Expr::Var(name))
            }
            Tok::Ident(name) => self.parse_ident_atom(name),
            _ => self.err(format!("expected an expression, found {}", self.describe())),
        }
    }

    fn parse_ident_atom(&mut self, name: String) -> Result<Expr> {
        let mut lower = name.to_ascii_lowercase();
        // A namespaced function: `duration.between(…)`, `datetime.fromepoch(…)`.
        let mut k = 1;
        while *self.peek_at(k) == Tok::Dot && matches!(self.peek_at(k + 1), Tok::Ident(_)) {
            k += 2;
        }
        if k > 1 && *self.peek_at(k) == Tok::LParen {
            for _ in 0..(k - 1) / 2 {
                self.advance();
                self.advance();
                if let Tok::Ident(part) = self.peek_at(0).clone() {
                    lower.push('.');
                    lower.push_str(&part.to_ascii_lowercase());
                }
            }
        }
        match lower.as_str() {
            "true" => {
                self.advance();
                return Ok(Expr::Literal(Value::Bool(true)));
            }
            "false" => {
                self.advance();
                return Ok(Expr::Literal(Value::Bool(false)));
            }
            "null" => {
                self.advance();
                return Ok(Expr::Literal(Value::Null));
            }
            "case" => return self.parse_case(),
            "count"
                if *self.peek_at(1) == Tok::LParen
                    && *self.peek_at(2) == Tok::Star
                    && *self.peek_at(3) == Tok::RParen =>
            {
                for _ in 0..4 {
                    self.advance();
                }
                return Ok(Expr::CountStar);
            }
            "all" | "any" | "none" | "single"
                if *self.peek_at(1) == Tok::LParen
                    && matches!(self.peek_at(2), Tok::Ident(_) | Tok::QuotedIdent(_))
                    && self.kw_at(3, "IN") =>
            {
                let kind = match lower.as_str() {
                    "all" => Quantifier::All,
                    "any" => Quantifier::Any,
                    "none" => Quantifier::None,
                    _ => Quantifier::Single,
                };
                self.advance();
                self.advance();
                let var = self.ident()?;
                self.expect_kw("IN")?;
                let list = self.parse_expr()?;
                self.expect_kw("WHERE")?;
                let pred = self.parse_expr()?;
                self.expect(Tok::RParen, "')'")?;
                return Ok(Expr::Quantified {
                    kind,
                    var,
                    list: Box::new(list),
                    pred: Box::new(pred),
                });
            }
            "reduce"
                if *self.peek_at(1) == Tok::LParen
                    && matches!(self.peek_at(2), Tok::Ident(_) | Tok::QuotedIdent(_))
                    && *self.peek_at(3) == Tok::Eq =>
            {
                self.advance();
                self.advance();
                let acc = self.ident()?;
                self.expect(Tok::Eq, "'=' after the accumulator")?;
                let init = self.parse_expr()?;
                self.expect(Tok::Comma, "',' after the initial value")?;
                let var = self.ident()?;
                self.expect_kw("IN")?;
                let list = self.parse_expr()?;
                self.expect(Tok::Pipe, "'|' before the reduce expression")?;
                let expr = self.parse_expr()?;
                self.expect(Tok::RParen, "')' to close reduce")?;
                return Ok(Expr::Reduce {
                    acc,
                    init: Box::new(init),
                    var,
                    list: Box::new(list),
                    expr: Box::new(expr),
                });
            }
            "exists" | "count" | "collect" if *self.peek_at(1) == Tok::LBrace => {
                let kind = match lower.as_str() {
                    "exists" => SubqueryKind::Exists,
                    "count" => SubqueryKind::Count,
                    _ => SubqueryKind::Collect,
                };
                self.advance();
                return self.parse_subquery_expr(kind);
            }
            _ => {}
        }
        if *self.peek_at(1) == Tok::LParen {
            self.advance();
            self.advance();
            let distinct = self.eat_kw("DISTINCT");
            let mut args = Vec::new();
            if !self.eat(&Tok::RParen) {
                args = self.parse_expr_list()?;
                self.expect(Tok::RParen, "')' to close the function call")?;
            }
            // `normalize(s, NFKD)`: the normal form is a keyword, not a variable.
            if lower == "normalize"
                && let [_, Expr::Var(name)] = args.as_slice()
                && let Some(f) = normal_form(name)
            {
                args[1] = Expr::Literal(Value::String(f.into()));
            }
            return Ok(Expr::Function {
                name: lower,
                distinct,
                args,
            });
        }
        self.advance();
        if *self.peek() == Tok::LBrace {
            return self.parse_map_projection(Expr::Var(name));
        }
        Ok(Expr::Var(name))
    }

    /// `{.key, .*, key: expr, var, …}` after the variable `base`.
    fn parse_map_projection(&mut self, base: Expr) -> Result<Expr> {
        self.expect(Tok::LBrace, "'{'")?;
        let mut items = Vec::new();
        if !self.eat(&Tok::RBrace) {
            loop {
                if self.eat(&Tok::Dot) {
                    if self.eat(&Tok::Star) {
                        items.push(MapProjectionItem::AllProperties);
                    } else {
                        items.push(MapProjectionItem::Property(self.ident()?));
                    }
                } else if *self.peek_at(1) == Tok::Colon {
                    let key = match self.peek().clone() {
                        Tok::Str(s) => {
                            self.advance();
                            s
                        }
                        _ => self.ident()?,
                    };
                    self.advance(); // ':'
                    items.push(MapProjectionItem::Entry(key, self.parse_expr()?));
                } else {
                    items.push(MapProjectionItem::Var(self.ident()?));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RBrace, "'}' to close the map projection")?;
        }
        Ok(Expr::MapProjection {
            base: Box::new(base),
            items,
        })
    }

    /// After `[`: a pattern comprehension `[p = (a)-->(b) WHERE … | expr]`, if one starts
    /// here. It runs as `COLLECT { MATCH p = (a)-->(b) WHERE … RETURN expr }`.
    fn try_pattern_comprehension(&mut self) -> Result<Option<Expr>> {
        let named = self.at_ident() && *self.peek_at(1) == Tok::Eq;
        if *self.peek() != Tok::LParen && !named {
            return Ok(None);
        }
        let start = self.pos;
        let part = match self.parse_pattern_part() {
            Ok(p) if !p.chain.is_empty() && (self.is_kw("WHERE") || *self.peek() == Tok::Pipe) => p,
            _ => {
                self.pos = start;
                return Ok(None);
            }
        };
        let where_ = if self.eat_kw("WHERE") {
            let outer = std::mem::replace(&mut self.pipe_ends_where, true);
            let w = self.parse_expr();
            self.pipe_ends_where = outer;
            Some(w?)
        } else {
            None
        };
        self.expect(Tok::Pipe, "'|' in the pattern comprehension")?;
        let expr = self.parse_expr()?;
        let mut names: Vec<String> = Vec::new();
        for t in &self.toks[start..self.pos] {
            if let Tok::Ident(n) | Tok::QuotedIdent(n) = &t.tok
                && !names.contains(n)
            {
                names.push(n.clone());
            }
        }
        self.expect(Tok::RBracket, "']' to close the pattern comprehension")?;
        let query = Query {
            parts: vec![SingleQuery {
                clauses: vec![
                    Clause::Match {
                        optional: false,
                        patterns: vec![part],
                        where_,
                    },
                    Clause::Return(Projection {
                        distinct: false,
                        star: false,
                        items: vec![ProjectionItem {
                            expr,
                            alias: "\u{0}value".into(),
                        }],
                        order_by: Vec::new(),
                        skip: None,
                        limit: None,
                        where_: None,
                    }),
                ],
            }],
            union_all: false,
            mode: QueryMode::Run,
        };
        Ok(Some(Expr::Subquery {
            kind: SubqueryKind::Collect,
            query: Box::new(query),
            names,
        }))
    }

    fn parse_list(&mut self) -> Result<Expr> {
        self.expect(Tok::LBracket, "'['")?;
        if let Some(e) = self.try_pattern_comprehension()? {
            return Ok(e);
        }
        if self.at_ident() && self.kw_at(1, "IN") {
            let var = self.ident()?;
            self.advance(); // IN
            let list = self.parse_expr()?;
            let filter = if self.eat_kw("WHERE") {
                let outer = std::mem::replace(&mut self.pipe_ends_where, true);
                let w = self.parse_expr();
                self.pipe_ends_where = outer;
                Some(Box::new(w?))
            } else {
                None
            };
            let map = if self.eat(&Tok::Pipe) {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect(Tok::RBracket, "']'")?;
            return Ok(Expr::ListComprehension {
                var,
                list: Box::new(list),
                filter,
                map,
            });
        }
        if self.eat(&Tok::RBracket) {
            return Ok(Expr::List(Vec::new()));
        }
        let items = self.parse_expr_list()?;
        self.expect(Tok::RBracket, "']' to close the list")?;
        Ok(Expr::List(items))
    }

    fn parse_map_literal(&mut self) -> Result<Expr> {
        self.expect(Tok::LBrace, "'{'")?;
        let mut items = Vec::new();
        if self.eat(&Tok::RBrace) {
            return Ok(Expr::Map(items));
        }
        loop {
            let key = match self.peek().clone() {
                Tok::Str(s) => {
                    self.advance();
                    s
                }
                _ => self.ident()?,
            };
            self.expect(Tok::Colon, "':' after map key")?;
            items.push((key, self.parse_expr()?));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace, "'}' to close the map")?;
        Ok(Expr::Map(items))
    }

    fn parse_case(&mut self) -> Result<Expr> {
        self.expect_kw("CASE")?;
        let subject = if self.is_kw("WHEN") {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw("WHEN") {
            let w = self.parse_expr()?;
            self.expect_kw("THEN")?;
            whens.push((w, self.parse_expr()?));
        }
        if whens.is_empty() {
            return self.err("CASE requires at least one WHEN branch");
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_kw("END")?;
        Ok(Expr::Case {
            subject,
            whens,
            else_,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(src: &str) -> Vec<Clause> {
        let q = parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(q.parts.len(), 1);
        q.parts.into_iter().next().unwrap().clauses
    }

    #[test]
    fn match_return() {
        let c = single(
            "MATCH (a:Person {name: 'Ann'})-[r:KNOWS|LIKES*1..3]->(b) WHERE b.age > 30 RETURN a.name AS n, b",
        );
        assert_eq!(c.len(), 2);
        let Clause::Match {
            patterns,
            where_,
            optional,
        } = &c[0]
        else {
            panic!()
        };
        assert!(!optional);
        assert!(where_.is_some());
        let p = &patterns[0];
        assert_eq!(p.start.labels, vec!["Person"]);
        let (rel, end) = &p.chain[0];
        assert_eq!(rel.types, vec!["KNOWS", "LIKES"]);
        assert_eq!(rel.direction, RelDirection::Right);
        assert_eq!(
            rel.length,
            Some(VarLength {
                min: 1,
                max: Some(3)
            })
        );
        assert_eq!(end.var.as_deref(), Some("b"));
        let Clause::Return(proj) = &c[1] else {
            panic!()
        };
        assert_eq!(proj.items[0].alias, "n");
        assert_eq!(proj.items[1].alias, "b");
    }

    #[test]
    fn default_alias_is_source_text() {
        let c = single("RETURN 1 + 2, count(*)");
        let Clause::Return(proj) = &c[0] else {
            panic!()
        };
        assert_eq!(proj.items[0].alias, "1 + 2");
        assert_eq!(proj.items[1].alias, "count(*)");
        assert_eq!(proj.items[1].expr, Expr::CountStar);
    }

    #[test]
    fn precedence() {
        let e = parse_expression("1 + 2 * 3 = 7 AND NOT false").unwrap();
        let Expr::Binary(BinaryOp::And, l, r) = e else {
            panic!()
        };
        assert!(matches!(*l, Expr::Binary(BinaryOp::Eq, _, _)));
        assert!(matches!(*r, Expr::Unary(UnaryOp::Not, _)));
    }

    #[test]
    fn chained_comparison() {
        let e = parse_expression("1 < x <= 5").unwrap();
        assert!(matches!(e, Expr::Binary(BinaryOp::And, _, _)));
    }

    #[test]
    fn pattern_predicate_vs_parens() {
        assert!(matches!(
            parse_expression("(a)-[:R]->(b)").unwrap(),
            Expr::PatternPredicate(_)
        ));
        assert!(matches!(
            parse_expression("(a) - 1").unwrap(),
            Expr::Binary(BinaryOp::Sub, _, _)
        ));
        assert!(matches!(
            parse_expression("(a.x + 1)").unwrap(),
            Expr::Binary(BinaryOp::Add, _, _)
        ));
    }

    #[test]
    fn comprehension_and_quantifier() {
        assert!(matches!(
            parse_expression("[x IN range(1, 5) WHERE x % 2 = 0 | x * x]").unwrap(),
            Expr::ListComprehension { .. }
        ));
        assert!(matches!(
            parse_expression("all(x IN [1,2] WHERE x > 0)").unwrap(),
            Expr::Quantified { .. }
        ));
        assert!(matches!(
            parse_expression("reduce(s = 0, x IN [1, 2] | s + x)").unwrap(),
            Expr::Reduce { .. }
        ));
    }

    #[test]
    fn writes() {
        let c = single(
            "MERGE (p:Person {name: $name}) ON CREATE SET p.created = 1 ON MATCH SET p += {seen: true}, p:Seen \
             WITH p REMOVE p.tmp, p:Old DETACH DELETE p",
        );
        assert_eq!(c.len(), 4);
        let Clause::Merge {
            on_create,
            on_match,
            ..
        } = &c[0]
        else {
            panic!()
        };
        assert_eq!(on_create.len(), 1);
        assert_eq!(on_match.len(), 2);
    }

    #[test]
    fn index_commands() {
        let c = single("CREATE INDEX person_name IF NOT EXISTS FOR (p:Person) ON (p.name)");
        assert_eq!(
            c[0],
            Clause::CreateIndex {
                name: Some("person_name".into()),
                entity: IndexEntity::Node,
                labels: vec!["Person".into()],
                properties: vec!["name".into()],
                if_not_exists: true,
                kind: IndexType::Range,
                options: None,
            }
        );
        let c = single("CREATE INDEX ON :Person(age)");
        assert!(
            matches!(&c[0], Clause::CreateIndex { name: None, properties, .. } if properties == &["age"])
        );
        assert_eq!(
            single("CREATE INDEX FOR (p:Person) ON (p.first, p.last)")[0],
            Clause::CreateIndex {
                name: None,
                entity: IndexEntity::Node,
                labels: vec!["Person".into()],
                properties: vec!["first".into(), "last".into()],
                if_not_exists: false,
                kind: IndexType::Range,
                options: None,
            }
        );
        for q in [
            "CREATE INDEX since FOR ()-[r:KNOWS]-() ON (r.since)",
            "CREATE INDEX since FOR ()-[r:KNOWS]->() ON (r.since)",
            "CREATE INDEX since FOR ()<-[r:KNOWS]-() ON (r.since)",
        ] {
            assert_eq!(
                single(q)[0],
                Clause::CreateIndex {
                    name: Some("since".into()),
                    entity: IndexEntity::Relationship,
                    labels: vec!["KNOWS".into()],
                    properties: vec!["since".into()],
                    if_not_exists: false,
                    kind: IndexType::Range,
                    options: None,
                },
                "{q}"
            );
        }
        for bad in [
            "CREATE INDEX FOR ()-[:KNOWS]-() ON (r.since)",
            "CREATE INDEX FOR ()-[r]-() ON (r.since)",
            "CREATE INDEX FOR ()-[r:A|B]-() ON (r.since)",
            "CREATE INDEX FOR ()-[r:A*]-() ON (r.since)",
            "CREATE INDEX FOR (p:P) ON (p.a, q.b)",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(matches!(
            &single("DROP INDEX ON :Person(first, last)")[0],
            Clause::DropIndex { target: IndexTarget::On { properties, .. }, .. } if properties.len() == 2
        ));
        let c = single("CREATE RANGE INDEX FOR (n:L) ON (n.k)");
        assert!(matches!(&c[0], Clause::CreateIndex { name: None, .. }));
        for q in [
            "CREATE VECTOR INDEX emb IF NOT EXISTS FOR (d:Doc) ON d.embedding OPTIONS {indexConfig: {`vector.dimensions`: 3}}",
            "CREATE VECTOR INDEX emb FOR (d:Doc) ON (d.embedding) OPTIONS $opts",
        ] {
            assert!(
                matches!(&single(q)[0], Clause::CreateIndex { kind: IndexType::Vector, options: Some(_), properties, .. } if properties == &["embedding"]),
                "{q}"
            );
        }
        assert!(matches!(
            &single("CREATE VECTOR INDEX FOR ()-[r:SIMILAR]-() ON r.v")[0],
            Clause::CreateIndex { kind: IndexType::Vector, entity: IndexEntity::Relationship, options: None, .. }
        ));
        assert!(matches!(
            &single("CREATE POINT INDEX places IF NOT EXISTS FOR (p:Place) ON (p.location)")[0],
            Clause::CreateIndex { name: Some(n), kind: IndexType::Point, entity: IndexEntity::Node, labels, properties, if_not_exists: true, .. }
                if n == "places" && labels == &["Place"] && properties == &["location"]
        ));
        assert!(matches!(
            &single("CREATE POINT INDEX FOR ()-[r:ROUTE]-() ON (r.waypoint) OPTIONS {indexConfig: {}}")[0],
            Clause::CreateIndex { kind: IndexType::Point, entity: IndexEntity::Relationship, options: Some(_), .. }
        ));
        let c = single("CREATE FULLTEXT INDEX titles IF NOT EXISTS FOR (n:Movie|Book) ON EACH [n.title, n.tagline] OPTIONS {indexConfig: {}}");
        assert!(matches!(
            &c[0],
            Clause::CreateIndex { name: Some(n), entity: IndexEntity::Node, labels, properties, if_not_exists: true, kind: IndexType::Fulltext, options: Some(_) }
                if n == "titles" && labels == &["Movie", "Book"] && properties == &["title", "tagline"]
        ));
        assert!(matches!(
            &single("CREATE FULLTEXT INDEX FOR ()-[r:REVIEWED|RATED]-() ON EACH [r.summary]")[0],
            Clause::CreateIndex { kind: IndexType::Fulltext, entity: IndexEntity::Relationship, labels, .. } if labels.len() == 2
        ));
        for bad in [
            "CREATE FULLTEXT INDEX FOR (n:Movie) ON (n.title)",
            "CREATE FULLTEXT INDEX FOR (n:Movie) ON EACH n.title",
            "CREATE FULLTEXT INDEX FOR (n:Movie) ON EACH [n.title",
            "CREATE FULLTEXT INDEX ON :Movie(title)",
            "CREATE FULLTEXT INDEX FOR ()-[r]-() ON EACH [r.title]",
            "CREATE INDEX FOR (n:A|B) ON (n.title)",
            "CREATE TEXT INDEX FOR (n:Movie) ON (n.title)",
            "CREATE POINT INDEX ON :Place(location)",
            "CREATE POINT INDEX FOR (n:A|B) ON (n.location)",
            "CREATE POINT INDEX FOR ()-[r:A|B]-() ON (r.location)",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        for bad in [
            "CREATE VECTOR INDEX ON :Doc(embedding)",
            "CREATE VECTOR INDEX FOR (d:Doc) ON d.a, d.b",
            "CREATE VECTOR INDEX FOR (d:Doc) ON d.a OPTIONS 3",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            single("DROP INDEX person_name IF EXISTS")[0],
            Clause::DropIndex {
                target: IndexTarget::Name("person_name".into()),
                if_exists: true
            }
        );
        assert!(matches!(
            single("DROP INDEX ON :Person(age)")[0],
            Clause::DropIndex { .. }
        ));
        assert_eq!(single("SHOW INDEXES")[0], Clause::ShowIndexes);
        assert_eq!(single("SHOW CONSTRAINTS")[0], Clause::ShowConstraints);
        assert_eq!(
            single("CREATE CONSTRAINT person_name IF NOT EXISTS FOR (p:Person) REQUIRE p.name IS UNIQUE")[0],
            Clause::CreateConstraint {
                name: Some("person_name".into()),
                entity: IndexEntity::Node,
                kind: ConstraintKind::Unique,
                label: "Person".into(),
                properties: vec!["name".into()],
                if_not_exists: true,
            }
        );
        assert!(matches!(
            &single("CREATE CONSTRAINT FOR (p:P) REQUIRE (p.a, p.b) IS NODE KEY")[0],
            Clause::CreateConstraint { name: None, kind: ConstraintKind::Key, properties, .. } if properties.len() == 2
        ));
        assert!(matches!(
            single("CREATE CONSTRAINT ON (p:P) ASSERT p.a IS NOT NULL")[0],
            Clause::CreateConstraint { kind: ConstraintKind::NotNull, .. }
        ));
        let constraint = |q: &str| match single(q).remove(0) {
            Clause::CreateConstraint { entity, kind, label, properties, .. } => (entity, kind, label, properties),
            other => panic!("{other:?}"),
        };
        let rel = IndexEntity::Relationship;
        assert_eq!(
            constraint("CREATE CONSTRAINT FOR ()-[r:KNOWS]->() REQUIRE r.since IS UNIQUE"),
            (rel, ConstraintKind::Unique, "KNOWS".into(), vec!["since".into()])
        );
        assert_eq!(constraint("CREATE CONSTRAINT FOR ()<-[r:R]-() REQUIRE (r.a, r.b) IS RELATIONSHIP KEY").1, ConstraintKind::Key);
        assert_eq!(constraint("CREATE CONSTRAINT FOR ()-[r:R]-() REQUIRE r.a IS REL UNIQUE").1, ConstraintKind::Unique);
        assert_eq!(constraint("CREATE CONSTRAINT FOR (n:N) REQUIRE n.a IS NODE UNIQUE").1, ConstraintKind::Unique);
        assert_eq!(constraint("CREATE CONSTRAINT FOR ()-[r:R]-() REQUIRE r.a IS NOT NULL").1, ConstraintKind::NotNull);
        let typed = |q: &str| match constraint(q).1 {
            ConstraintKind::Typed(t) => t.to_string(),
            other => panic!("{other:?}"),
        };
        assert_eq!(typed("CREATE CONSTRAINT FOR (n:N) REQUIRE n.a IS :: INTEGER"), "INTEGER");
        assert_eq!(typed("CREATE CONSTRAINT FOR (n:N) REQUIRE n.a :: INT | FLOAT"), "INTEGER | FLOAT");
        assert_eq!(typed("CREATE CONSTRAINT FOR (n:N) REQUIRE n.a IS TYPED ANY<STRING | LIST<STRING NOT NULL>>"), "STRING | LIST<STRING NOT NULL>");
        assert_eq!(
            typed("CREATE CONSTRAINT FOR ()-[r:R]-() REQUIRE r.a IS :: TIMESTAMP WITH TIME ZONE | LOCAL TIME | SIGNED INTEGER"),
            "INTEGER | LOCAL TIME | ZONED DATETIME"
        );
        assert_eq!(
            single("DROP CONSTRAINT c IF EXISTS")[0],
            Clause::DropConstraint { name: "c".into(), if_exists: true }
        );
        for bad in [
            "CREATE CONSTRAINT FOR (p:P) REQUIRE q.a IS UNIQUE",
            "CREATE CONSTRAINT FOR (p:P) REQUIRE p.a IS",
            "CREATE CONSTRAINT FOR (p:P) REQUIRE p.a, p.b IS UNIQUE",
            "CREATE CONSTRAINT FOR ()-[r]-() REQUIRE r.a IS UNIQUE",
            "CREATE CONSTRAINT FOR ()-[r:A|B]-() REQUIRE r.a IS UNIQUE",
            "CREATE CONSTRAINT FOR ()-[r:R]-() REQUIRE r.a IS NODE KEY",
            "CREATE CONSTRAINT FOR (p:P) REQUIRE p.a IS RELATIONSHIP KEY",
            "CREATE CONSTRAINT FOR (p:P) REQUIRE p.a IS :: INTEGER NOT NULL",
            "CREATE CONSTRAINT FOR (p:P) REQUIRE p.a IS :: LIST<INTEGER>",
            "CREATE CONSTRAINT FOR (p:P) REQUIRE p.a IS :: MAP",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(matches!(
            parse("CREATE INDEX FOR (n:L) ON (m.k)"),
            Err(Error::Syntax { .. })
        ));
    }

    #[test]
    fn union() {
        let q = parse("RETURN 1 AS x UNION ALL RETURN 2 AS x;").unwrap();
        assert_eq!(q.parts.len(), 2);
        assert!(q.union_all);
        assert!(parse("RETURN 1 AS x UNION RETURN 2 AS x UNION ALL RETURN 3 AS x").is_err());
    }

    #[test]
    fn database_commands() {
        let db = |s: &str| match parse_statement(s).unwrap() {
            Statement::Database(c) => c,
            other => panic!("not a database command: {other:?}"),
        };
        let name = |s: &str| DbName::Name(s.into());
        assert_eq!(
            db("CREATE DATABASE movies"),
            DatabaseCommand::Create {
                name: name("movies"),
                existing: ExistingDatabase::Fail,
                options: None
            }
        );
        assert!(matches!(
            db("create or replace database `my-db` OPTIONS {storage: 'memory'} WAIT;"),
            DatabaseCommand::Create {
                name: DbName::Name(n),
                existing: ExistingDatabase::Replace,
                options: Some(Expr::Map(_))
            } if n == "my-db"
        ));
        assert!(matches!(
            db("CREATE DATABASE a.b.c IF NOT EXISTS OPTIONS $opts NOWAIT"),
            DatabaseCommand::Create {
                name: DbName::Name(n),
                existing: ExistingDatabase::Keep,
                options: Some(Expr::Param(_))
            } if n == "a.b.c"
        ));
        assert!(matches!(
            parse_statement("CREATE OR REPLACE DATABASE x IF NOT EXISTS"),
            Err(Error::Syntax { .. })
        ));
        assert_eq!(
            db("DROP DATABASE $n IF EXISTS DESTROY DATA"),
            DatabaseCommand::Drop {
                name: DbName::Param("n".into()),
                if_exists: true,
                dump: false,
            }
        );
        assert_eq!(
            db("DROP DATABASE x DUMP DATA NOWAIT"),
            DatabaseCommand::Drop { name: name("x"), if_exists: false, dump: true }
        );
        assert_eq!(
            db("ALTER DATABASE x IF EXISTS SET ACCESS READ ONLY WAIT"),
            DatabaseCommand::Alter { name: name("x"), if_exists: true, read_only: true }
        );
        assert_eq!(
            db("ALTER DATABASE x SET ACCESS READ WRITE"),
            DatabaseCommand::Alter { name: name("x"), if_exists: false, read_only: false }
        );
        assert_eq!(
            db("CREATE ALIAS films IF NOT EXISTS FOR DATABASE movies"),
            DatabaseCommand::CreateAlias { alias: name("films"), target: name("movies"), existing: ExistingDatabase::Keep }
        );
        assert_eq!(
            db("CREATE OR REPLACE ALIAS a.b FOR DATABASE $db"),
            DatabaseCommand::CreateAlias {
                alias: name("a.b"),
                target: DbName::Param("db".into()),
                existing: ExistingDatabase::Replace
            }
        );
        assert_eq!(
            db("ALTER ALIAS films IF EXISTS SET DATABASE TARGET other"),
            DatabaseCommand::AlterAlias { alias: name("films"), if_exists: true, target: name("other") }
        );
        assert_eq!(
            db("DROP ALIAS films IF EXISTS FOR DATABASE"),
            DatabaseCommand::DropAlias { alias: name("films"), if_exists: true }
        );
        assert_eq!(db("SHOW ALIASES FOR DATABASES"), DatabaseCommand::ShowAliases(None));
        assert_eq!(db("SHOW ALIAS films FOR DATABASE"), DatabaseCommand::ShowAliases(Some(name("films"))));
        for (q, unsupported) in [
            ("ALTER DATABASE x SET TOPOLOGY 1 PRIMARY", true),
            ("ALTER DATABASE x REMOVE OPTION txLogEnrichment", true),
            ("ALTER DATABASE x SET ACCESS READ", false),
            ("CREATE ALIAS a FOR DATABASE b AT 'neo4j+s://x'", true),
            ("CREATE OR REPLACE ALIAS a IF NOT EXISTS FOR DATABASE b", false),
            ("DROP ALIAS a", false),
        ] {
            match parse_statement(q) {
                Err(Error::Unsupported(_)) if unsupported => {}
                Err(Error::Syntax { .. }) if !unsupported => {}
                other => panic!("{q}: {other:?}"),
            }
        }
        assert_eq!(
            db("SHOW DATABASES"),
            DatabaseCommand::Show(ShowDatabases::All)
        );
        assert_eq!(
            db("SHOW DATABASE;"),
            DatabaseCommand::Show(ShowDatabases::All)
        );
        assert_eq!(
            db("SHOW DATABASE x"),
            DatabaseCommand::Show(ShowDatabases::Named(name("x")))
        );
        assert_eq!(
            db("SHOW DEFAULT DATABASE"),
            DatabaseCommand::Show(ShowDatabases::Default)
        );

        assert_eq!(
            parse_statement("USE movies;").unwrap(),
            Statement::Use(name("movies"))
        );
        assert!(matches!(
            parse_statement("USE movies MATCH (n) RETURN n").unwrap(),
            Statement::Query {
                use_db: Some(_),
                ..
            }
        ));
        // A plain CREATE is still a pattern; single-database parsing rejects commands.
        assert!(matches!(
            parse_statement("CREATE (n)").unwrap(),
            Statement::Query { use_db: None, .. }
        ));
        assert!(matches!(
            parse("CREATE DATABASE x"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            parse("USE x RETURN 1"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn user_commands() {
        let user = |s: &str| match parse_statement(s).unwrap() {
            Statement::User(c) => c,
            other => panic!("not a user command: {other:?}"),
        };
        let name = |s: &str| DbName::Name(s.into());
        let lit = |s: &str| Secret::Literal(s.into());
        assert_eq!(user("SHOW USERS"), UserCommand::ShowUsers);
        assert_eq!(user("show current user;"), UserCommand::ShowCurrentUser);
        assert_eq!(user("SHOW ROLES"), UserCommand::ShowRoles);
        assert_eq!(
            user("CREATE USER alice SET PASSWORD 'secret' CHANGE NOT REQUIRED"),
            UserCommand::Create {
                name: name("alice"),
                existing: ExistingUser::Fail,
                password: lit("secret")
            }
        );
        assert_eq!(
            user("CREATE OR REPLACE USER `a.b@c` SET PLAINTEXT PASSWORD $pw SET STATUS ACTIVE"),
            UserCommand::Create {
                name: name("a.b@c"),
                existing: ExistingUser::Replace,
                password: Secret::Param("pw".into())
            }
        );
        assert_eq!(
            user("CREATE USER $u IF NOT EXISTS SET PASSWORD 'x'"),
            UserCommand::Create {
                name: DbName::Param("u".into()),
                existing: ExistingUser::Keep,
                password: lit("x")
            }
        );
        assert_eq!(
            user("ALTER USER bob IF EXISTS SET PASSWORD 'new'"),
            UserCommand::Alter {
                name: name("bob"),
                if_exists: true,
                password: Some(lit("new"))
            }
        );
        assert_eq!(
            user("ALTER USER bob SET PASSWORD CHANGE NOT REQUIRED"),
            UserCommand::Alter {
                name: name("bob"),
                if_exists: false,
                password: None
            }
        );
        assert_eq!(
            user("ALTER CURRENT USER SET PASSWORD FROM 'a' TO $b"),
            UserCommand::AlterCurrent {
                old: lit("a"),
                new: Secret::Param("b".into())
            }
        );
        assert_eq!(
            user("RENAME USER a IF EXISTS TO b"),
            UserCommand::Rename {
                from: name("a"),
                to: name("b"),
                if_exists: true
            }
        );
        assert_eq!(
            user("DROP USER a IF EXISTS"),
            UserCommand::Drop {
                name: name("a"),
                if_exists: true
            }
        );
        assert_eq!(
            user("GRANT ROLES reader, editor TO a, $b"),
            UserCommand::Grant {
                roles: vec![name("reader"), name("editor")],
                database: None,
                users: vec![name("a"), DbName::Param("b".into())]
            }
        );
        assert_eq!(
            user("REVOKE ROLE admin FROM a"),
            UserCommand::Revoke {
                roles: vec![name("admin")],
                database: None,
                users: vec![name("a")]
            }
        );
        assert_eq!(
            user("GRANT ROLE reader ON DATABASE movies.v2 TO a"),
            UserCommand::Grant { roles: vec![name("reader")], database: Some(name("movies.v2")), users: vec![name("a")] }
        );
        assert_eq!(
            user("REVOKE ROLE editor ON DATABASE $db FROM a"),
            UserCommand::Revoke {
                roles: vec![name("editor")],
                database: Some(DbName::Param("db".into())),
                users: vec![name("a")]
            }
        );
        assert_eq!(
            user("GRANT ROLE reader ON DATABASES * TO a"),
            UserCommand::Grant { roles: vec![name("reader")], database: None, users: vec![name("a")] }
        );
        for bad in ["GRANT ROLE reader ON GRAPH g TO a", "GRANT ROLE reader ON DATABASE TO a", "GRANT ROLE reader ON DATABASES g TO a"] {
            assert!(parse_statement(bad).is_err(), "{bad}");
        }
        for unsupported in [
            "CREATE USER a SET PASSWORD 'x' CHANGE REQUIRED",
            "CREATE USER a SET ENCRYPTED PASSWORD 'x'",
            "ALTER USER a SET STATUS SUSPENDED",
            "ALTER USER a SET HOME DATABASE db",
            "SHOW ROLES WITH USERS",
        ] {
            assert!(matches!(parse_statement(unsupported), Err(Error::Unsupported(_))), "{unsupported}");
        }
        for bad in [
            "CREATE USER a",
            "CREATE OR REPLACE USER a IF NOT EXISTS SET PASSWORD 'x'",
            "ALTER USER a",
            "GRANT ROLE r",
            "DROP USER a extra",
            "SHOW USERS YIELD user",
        ] {
            assert!(matches!(parse_statement(bad), Err(Error::Syntax { .. })), "{bad}");
        }
        // Patterns and database commands are unaffected.
        assert!(matches!(parse_statement("CREATE (user:User)").unwrap(), Statement::Query { .. }));
        assert!(matches!(
            parse_statement("CREATE OR REPLACE DATABASE users").unwrap(),
            Statement::Database(_)
        ));
        assert!(matches!(parse("SHOW USERS"), Err(Error::Unsupported(_))));
        // Literal passwords stay out of debug output.
        assert!(!format!("{:?}", user("CREATE USER a SET PASSWORD 'hunter2'")).contains("hunter2"));
    }

    #[test]
    fn errors_have_offsets() {
        let err = parse("MATCH (n RETURN n").unwrap_err();
        assert!(matches!(err, Error::Syntax { offset: 9, .. }), "{err:?}");
    }

    #[test]
    fn transaction_commands() {
        let tx = |src: &str| match parse_statement(src).unwrap() {
            Statement::Transaction(c) => c,
            other => panic!("{src}: {other:?}"),
        };
        let c = tx("SHOW TRANSACTIONS");
        assert!(!c.terminate && c.ids.is_empty() && c.tail == ShowTail::default());
        let c = tx("show transaction 'a', $b");
        assert_eq!(c.ids, vec![Expr::Literal(Value::String("a".into())), Expr::Param("b".into())]);
        let c = tx("SHOW TRANSACTIONS WHERE username = 'ann'");
        assert!(c.tail.where_.is_some() && c.tail.yields.is_none());
        let c = tx("SHOW TRANSACTIONS YIELD transactionId AS id, status ORDER BY id LIMIT 2 WHERE status = 'Idle' RETURN id");
        let y = c.tail.yields.unwrap();
        assert_eq!((y.items.len(), y.order_by.len(), y.limit.is_some(), y.where_.is_some()), (2, 1, true, true));
        assert!(c.tail.returns.is_some());
        assert!(tx("SHOW TRANSACTIONS YIELD *").tail.yields.unwrap().star);
        let c = tx("TERMINATE TRANSACTIONS 'neo4j-transaction-1' YIELD message");
        assert!(c.terminate && c.ids.len() == 1);
        assert!(matches!(parse_statement("TERMINATE TRANSACTIONS"), Err(Error::Syntax { .. })));
        assert!(matches!(parse_statement("SHOW TRANSACTIONS RETURN 1"), Err(Error::Syntax { .. })));
        assert!(matches!(parse("SHOW TRANSACTIONS"), Err(Error::Unsupported(_))));
    }
}
