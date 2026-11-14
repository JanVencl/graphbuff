//! User accounts: names, password hashes and roles, kept in a text file.
//!
//! Passwords are stored as PBKDF2-HMAC-SHA-256 hashes with a random 16-byte salt
//! ([`DEFAULT_ITERATIONS`] rounds by default). A successful login is remembered (as a
//! fast salted hash) until the password changes, so clients that open many connections
//! pay the slow hash once.
//!
//! Each user has a set of roles; the strongest one decides what the user may do:
//!
//! | Role | May |
//! |------|-----|
//! | `reader` | run queries that only read, `SHOW DATABASES`, `EXPLAIN` |
//! | `editor` | also write: `CREATE`, `MERGE`, `SET`, `DELETE`, indexes and constraints |
//! | `admin`  | also create and drop databases, and manage users |
//!
//! A user with no role may only connect, run `SHOW CURRENT USER` and change their own
//! password.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use graphbuff::{Error, QueryResult, QueryStats, Result, ResultValue, Value};
use graphbuff::cypher::ast::{DbName, ExistingDatabase, Secret, UserCommand};

use crate::crypto::{self, Sha256};

/// PBKDF2 rounds for new passwords (OWASP's 2023 advice for PBKDF2-HMAC-SHA-256).
pub const DEFAULT_ITERATIONS: u32 = 600_000;

const HEADER: &str = "graphbuff users v1";
const SALT_BYTES: usize = 16;

/// A role a user may hold. Stronger roles include what weaker ones may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Reader,
    Editor,
    Admin,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Reader, Role::Editor, Role::Admin];

    pub fn name(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Editor => "editor",
            Role::Admin => "admin",
        }
    }

    /// Parses a role name (case-insensitive).
    pub fn parse(name: &str) -> Result<Role> {
        Role::ALL
            .into_iter()
            .find(|r| r.name().eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::Runtime(format!("role `{name}` does not exist; the roles are reader, editor and admin")))
    }

    fn access(self) -> Access {
        match self {
            Role::Reader => Access::Read,
            Role::Editor => Access::Write,
            Role::Admin => Access::Admin,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a client may do, from its strongest role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Access {
    /// Connect, `SHOW CURRENT USER`, change one's own password.
    None,
    Read,
    Write,
    Admin,
}

#[derive(Clone)]
struct PasswordHash {
    iterations: u32,
    salt: [u8; SALT_BYTES],
    hash: [u8; 32],
}

impl PasswordHash {
    fn new(password: &str, iterations: u32) -> Self {
        let mut salt = [0u8; SALT_BYTES];
        salt.copy_from_slice(&crypto::random_bytes()[..SALT_BYTES]);
        PasswordHash {
            iterations,
            salt,
            hash: crypto::pbkdf2_sha256(password.as_bytes(), &salt, iterations),
        }
    }

    fn matches(&self, password: &str) -> bool {
        crypto::same_bytes(
            &crypto::pbkdf2_sha256(password.as_bytes(), &self.salt, self.iterations),
            &self.hash,
        )
    }

    /// A fast salted hash of `password`, for the cache of verified logins.
    fn quick(&self, password: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(&self.salt);
        h.update(&self.hash);
        h.update(password.as_bytes());
        h.finish()
    }

    fn encode(&self) -> String {
        format!(
            "pbkdf2-sha256:{}:{}:{}",
            self.iterations,
            crypto::hex(&self.salt),
            crypto::hex(&self.hash)
        )
    }

    fn decode(s: &str) -> Option<Self> {
        let mut parts = s.split(':');
        let (Some("pbkdf2-sha256"), Some(iterations), Some(salt), Some(hash), None) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        Some(PasswordHash {
            iterations: iterations.parse().ok().filter(|&n| n > 0)?,
            salt: crypto::unhex(salt)?.try_into().ok()?,
            hash: crypto::unhex(hash)?.try_into().ok()?,
        })
    }
}

#[derive(Clone)]
struct Account {
    password: PasswordHash,
    roles: BTreeSet<Role>,
}

#[derive(Default)]
struct State {
    accounts: BTreeMap<String, Account>,
    /// Users whose password was checked: their [`PasswordHash::quick`] hash.
    verified: HashMap<String, [u8; 32]>,
}

/// A set of user accounts, optionally kept in a file. Changes are written to the file
/// (atomically, by renaming a new copy over it) before they take effect.
pub struct Users {
    path: Option<PathBuf>,
    iterations: u32,
    state: RwLock<State>,
}

impl fmt::Debug for Users {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Users")
            .field("path", &self.path)
            .field("users", &self.read().accounts.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Checks a user name: 1 to 64 characters, letters, digits and `_ . @ -`.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '@' | '-'));
    if ok {
        Ok(())
    } else {
        Err(Error::Semantic(format!(
            "invalid user name `{name}`: use 1 to 64 letters, digits, '_', '.', '@' and '-'"
        )))
    }
}

fn check_password(password: &str) -> Result<()> {
    if password.is_empty() {
        return Err(Error::Semantic("a password cannot be empty".into()));
    }
    Ok(())
}

fn no_user(name: &str) -> Error {
    Error::Runtime(format!("user `{name}` does not exist"))
}

impl Users {
    /// Accounts that live only as long as the server.
    pub fn in_memory() -> Users {
        Users {
            path: None,
            iterations: DEFAULT_ITERATIONS,
            state: RwLock::default(),
        }
    }

    /// Opens the accounts kept in `path`; a missing file is an empty set (the file is
    /// created on the first change).
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Users> {
        let path = path.into();
        let accounts = match fs::read_to_string(&path) {
            Ok(text) => parse(&text, &path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Users {
            path: Some(path),
            iterations: DEFAULT_ITERATIONS,
            state: RwLock::new(State {
                accounts,
                verified: HashMap::new(),
            }),
        })
    }

    /// Hashes new passwords with `iterations` PBKDF2 rounds instead of
    /// [`DEFAULT_ITERATIONS`] (fewer only for tests; existing hashes keep theirs).
    pub fn with_iterations(mut self, iterations: u32) -> Users {
        self.iterations = iterations.max(1);
        self
    }

    /// The file the accounts are kept in, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_empty(&self) -> bool {
        self.read().accounts.is_empty()
    }

    /// Every user with their roles, by name.
    pub fn list(&self) -> Vec<(String, Vec<Role>)> {
        self.read()
            .accounts
            .iter()
            .map(|(n, a)| (n.clone(), a.roles.iter().copied().collect()))
            .collect()
    }

    /// The roles of `name`, if the user exists.
    pub fn roles(&self, name: &str) -> Option<Vec<Role>> {
        self.read().accounts.get(name).map(|a| a.roles.iter().copied().collect())
    }

    /// What `name` may do; `None` if there is no such user (any more).
    pub(crate) fn access(&self, name: &str) -> Option<Access> {
        let st = self.read();
        let account = st.accounts.get(name)?;
        Some(account.roles.iter().map(|r| r.access()).max().unwrap_or(Access::None))
    }

    /// Whether `password` is `name`'s password.
    pub fn authenticate(&self, name: &str, password: &str) -> bool {
        let account = {
            let st = self.read();
            let Some(account) = st.accounts.get(name) else {
                return false;
            };
            if let Some(quick) = st.verified.get(name) {
                return crypto::same_bytes(quick, &account.password.quick(password));
            }
            account.password.clone()
        };
        // The slow hash runs without the lock.
        if !account.matches(password) {
            return false;
        }
        let mut st = self.write();
        // Remember it only if the password did not change meanwhile.
        if st.accounts.get(name).is_some_and(|a| a.password.hash == account.hash) {
            st.verified.insert(name.to_string(), account.quick(password));
        }
        true
    }

    /// Applies `change` to a copy of the accounts, saves the copy, then makes it
    /// current. Returns what `change` returned.
    fn change<T>(&self, change: impl FnOnce(&mut BTreeMap<String, Account>) -> Result<T>) -> Result<T> {
        let mut st = self.write();
        let mut accounts = st.accounts.clone();
        let out = change(&mut accounts)?;
        if let Some(path) = &self.path {
            save(path, &accounts).map_err(|e| Error::io(format!("writing {}", path.display()), e))?;
        }
        let State { accounts: old, verified } = &mut *st;
        verified.retain(|name, _| {
            matches!((old.get(name), accounts.get(name)), (Some(a), Some(b)) if a.password.hash == b.password.hash)
        });
        st.accounts = accounts;
        Ok(out)
    }

    /// Creates a user, or replaces an existing one when `replace` is set. Returns
    /// whether anything changed.
    pub fn create(&self, name: &str, password: &str, roles: &[Role], existing: ExistingDatabase) -> Result<bool> {
        check_name(name)?;
        check_password(password)?;
        if existing == ExistingDatabase::Keep && self.read().accounts.contains_key(name) {
            return Ok(false);
        }
        let password = PasswordHash::new(password, self.iterations);
        self.change(|accounts| {
            if accounts.contains_key(name) {
                match existing {
                    ExistingDatabase::Fail => return Err(Error::Runtime(format!("user `{name}` already exists"))),
                    ExistingDatabase::Keep => return Ok(false),
                    ExistingDatabase::Replace => {}
                }
            }
            let roles = roles.iter().copied().collect();
            accounts.insert(name.to_string(), Account { password, roles });
            Ok(true)
        })
    }

    /// Sets `name`'s password.
    pub fn set_password(&self, name: &str, password: &str) -> Result<()> {
        check_password(password)?;
        if !self.read().accounts.contains_key(name) {
            return Err(no_user(name));
        }
        let password = PasswordHash::new(password, self.iterations);
        self.change(|accounts| {
            accounts.get_mut(name).ok_or_else(|| no_user(name))?.password = password;
            Ok(())
        })
    }

    /// Gives `role` to `name`; returns whether the user lacked it.
    pub fn grant(&self, name: &str, role: Role) -> Result<bool> {
        self.change(|accounts| Ok(accounts.get_mut(name).ok_or_else(|| no_user(name))?.roles.insert(role)))
    }

    /// Takes `role` from `name`; returns whether the user had it.
    pub fn revoke(&self, name: &str, role: Role) -> Result<bool> {
        self.change(|accounts| Ok(accounts.get_mut(name).ok_or_else(|| no_user(name))?.roles.remove(&role)))
    }

    /// Removes a user; returns whether there was one.
    pub fn drop_user(&self, name: &str) -> Result<bool> {
        if !self.read().accounts.contains_key(name) {
            return Ok(false);
        }
        self.change(|accounts| Ok(accounts.remove(name).is_some()))
    }

    /// Renames a user; returns whether there was one to rename.
    pub fn rename(&self, from: &str, to: &str) -> Result<bool> {
        check_name(to)?;
        self.change(|accounts| {
            if !accounts.contains_key(from) {
                return Ok(false);
            }
            if accounts.contains_key(to) {
                return Err(Error::Runtime(format!("user `{to}` already exists")));
            }
            let account = accounts.remove(from).expect("checked");
            accounts.insert(to.to_string(), account);
            Ok(true)
        })
    }
}

fn parse(text: &str, path: &Path) -> io::Result<BTreeMap<String, Account>> {
    let bad = |line: usize, msg: &str| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{} line {line}: {msg}", path.display()))
    };
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l.trim()));
    match lines.next() {
        Some((_, HEADER)) => {}
        _ => return Err(bad(1, "not a graphbuff users file")),
    }
    let mut accounts = BTreeMap::new();
    for (n, line) in lines {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [] => {}
            [w, ..] if w.starts_with('#') => {}
            ["user", name, hash, roles] => {
                check_name(name).map_err(|_| bad(n, "invalid user name"))?;
                let password = PasswordHash::decode(hash).ok_or_else(|| bad(n, "invalid password hash"))?;
                let roles = roles
                    .strip_prefix("roles=")
                    .ok_or_else(|| bad(n, "expected roles=…"))?
                    .split(',')
                    .filter(|r| !r.is_empty())
                    .map(|r| Role::parse(r).map_err(|_| bad(n, &format!("unknown role `{r}`"))))
                    .collect::<io::Result<_>>()?;
                if accounts.insert(name.to_string(), Account { password, roles }).is_some() {
                    return Err(bad(n, "duplicate user"));
                }
            }
            _ => return Err(bad(n, "unrecognized line")),
        }
    }
    Ok(accounts)
}

fn save(path: &Path, accounts: &BTreeMap<String, Account>) -> io::Result<()> {
    let mut text = format!("{HEADER}\n");
    for (name, a) in accounts {
        let roles: Vec<&str> = a.roles.iter().map(|r| r.name()).collect();
        text.push_str(&format!("user {name} {} roles={}\n", a.password.encode(), roles.join(",")));
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

// ---- Cypher user commands -----------------------------------------------------------------

/// Who sent a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Principal {
    /// Authentication is off: anyone may do anything.
    Open,
    /// The server token: an administrator that is not a user account.
    Token,
    User(String),
}

fn resolve(name: &DbName, params: &BTreeMap<String, Value>, what: &str) -> Result<String> {
    match name {
        DbName::Name(n) => Ok(n.clone()),
        DbName::Param(p) => match params.get(p) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(Error::Type(format!("${p} must be a string {what}"))),
            None => Err(Error::Semantic(format!("missing parameter ${p}"))),
        },
    }
}

fn secret(s: &Secret, params: &BTreeMap<String, Value>) -> Result<String> {
    match s {
        Secret::Literal(v) => Ok(v.clone()),
        Secret::Param(p) => resolve(&DbName::Param(p.clone()), params, "password"),
    }
}

fn changed(n: usize) -> QueryResult {
    QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        stats: QueryStats {
            system_updates: n,
            ..Default::default()
        },
        plan: None,
    }
}

fn user_row(name: &str, roles: &[Role]) -> Vec<ResultValue> {
    vec![
        ResultValue::String(name.to_string()),
        ResultValue::List(roles.iter().map(|r| ResultValue::String(r.name().into())).collect()),
        ResultValue::Bool(false),
        ResultValue::Bool(false),
        ResultValue::Null,
    ]
}

fn user_table(rows: Vec<Vec<ResultValue>>) -> QueryResult {
    QueryResult {
        columns: ["user", "roles", "passwordChangeRequired", "suspended", "home"]
            .map(String::from)
            .to_vec(),
        rows,
        stats: QueryStats::default(),
        plan: None,
    }
}

/// Runs a user command for `who`, who may do `access`.
pub(crate) fn run(
    users: &Users,
    who: &Principal,
    access: Access,
    cmd: &UserCommand,
    params: &BTreeMap<String, Value>,
) -> Result<QueryResult> {
    let own = matches!(cmd, UserCommand::ShowCurrentUser | UserCommand::AlterCurrent { .. });
    if !own && access < Access::Admin {
        return Err(Error::Forbidden(format!("{} needs the admin role", cmd.name())));
    }
    match cmd {
        UserCommand::ShowUsers => Ok(user_table(users.list().iter().map(|(n, r)| user_row(n, r)).collect())),
        UserCommand::ShowCurrentUser => {
            let rows = match who {
                Principal::User(name) => users.roles(name).map(|r| user_row(name, &r)).into_iter().collect(),
                _ => Vec::new(),
            };
            Ok(user_table(rows))
        }
        UserCommand::ShowRoles => Ok(QueryResult {
            columns: vec!["role".into()],
            rows: Role::ALL.iter().map(|r| vec![ResultValue::String(r.name().into())]).collect(),
            stats: QueryStats::default(),
            plan: None,
        }),
        UserCommand::Create {
            name,
            existing,
            password,
        } => {
            let name = resolve(name, params, "user name")?;
            let created = users.create(&name, &secret(password, params)?, &[], *existing)?;
            Ok(changed(created as usize))
        }
        UserCommand::Alter {
            name,
            if_exists,
            password,
        } => {
            let name = resolve(name, params, "user name")?;
            if users.roles(&name).is_none() {
                return if *if_exists { Ok(changed(0)) } else { Err(no_user(&name)) };
            }
            match password {
                Some(p) => {
                    users.set_password(&name, &secret(p, params)?)?;
                    Ok(changed(1))
                }
                None => Ok(changed(0)),
            }
        }
        UserCommand::AlterCurrent { old, new } => {
            let Principal::User(name) = who else {
                return Err(Error::Runtime(
                    "ALTER CURRENT USER needs a user account; this client logged in with the server token or without authentication"
                        .into(),
                ));
            };
            let (old, new) = (secret(old, params)?, secret(new, params)?);
            if !users.authenticate(name, &old) {
                return Err(Error::Forbidden("the current password is wrong".into()));
            }
            if old == new {
                return Err(Error::Semantic("the new password must differ from the old one".into()));
            }
            users.set_password(name, &new)?;
            Ok(changed(1))
        }
        UserCommand::Rename { from, to, if_exists } => {
            let (from, to) = (resolve(from, params, "user name")?, resolve(to, params, "user name")?);
            match users.rename(&from, &to)? {
                true => Ok(changed(1)),
                false if *if_exists => Ok(changed(0)),
                false => Err(no_user(&from)),
            }
        }
        UserCommand::Drop { name, if_exists } => {
            let name = resolve(name, params, "user name")?;
            if *who == Principal::User(name.clone()) {
                return Err(Error::Runtime(format!("user `{name}` cannot drop their own account")));
            }
            match users.drop_user(&name)? {
                true => Ok(changed(1)),
                false if *if_exists => Ok(changed(0)),
                false => Err(no_user(&name)),
            }
        }
        UserCommand::Grant { roles, users: names } | UserCommand::Revoke { roles, users: names } => {
            let grant = matches!(cmd, UserCommand::Grant { .. });
            let roles = roles
                .iter()
                .map(|r| Role::parse(&resolve(r, params, "role name")?))
                .collect::<Result<Vec<_>>>()?;
            let names = names
                .iter()
                .map(|n| resolve(n, params, "user name"))
                .collect::<Result<Vec<_>>>()?;
            if let Some(missing) = names.iter().find(|n| users.roles(n).is_none()) {
                return Err(no_user(missing));
            }
            let mut n = 0;
            for name in &names {
                for &role in &roles {
                    let did = if grant { users.grant(name, role)? } else { users.revoke(name, role)? };
                    n += did as usize;
                }
            }
            Ok(changed(n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphbuff-users-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("users")
    }

    #[test]
    fn accounts_persist_and_authenticate() {
        let path = temp_file("persist");
        let users = Users::open(&path).unwrap().with_iterations(10);
        assert!(users.is_empty());
        assert!(users.create("alice", "pw1", &[Role::Admin], ExistingDatabase::Fail).unwrap());
        assert!(users.create("bob", "pw2", &[], ExistingDatabase::Fail).unwrap());
        assert!(users.create("bob", "x", &[], ExistingDatabase::Fail).is_err());
        assert!(!users.create("bob", "x", &[], ExistingDatabase::Keep).unwrap());
        assert!(users.grant("bob", Role::Reader).unwrap());
        assert!(!users.grant("bob", Role::Reader).unwrap());
        assert!(users.authenticate("alice", "pw1"));
        assert!(users.authenticate("alice", "pw1")); // from the cache
        assert!(!users.authenticate("alice", "pw2"));
        assert!(!users.authenticate("carol", "pw1"));

        let again = Users::open(&path).unwrap();
        assert_eq!(
            again.list(),
            vec![("alice".into(), vec![Role::Admin]), ("bob".into(), vec![Role::Reader])]
        );
        assert!(again.authenticate("bob", "pw2"));
        assert_eq!(again.access("bob"), Some(Access::Read));
        assert_eq!(again.access("nobody"), None);

        // A new password replaces the remembered login.
        users.set_password("alice", "new").unwrap();
        assert!(!users.authenticate("alice", "pw1"));
        assert!(users.authenticate("alice", "new"));
        assert!(users.rename("bob", "robert").unwrap());
        assert!(!users.authenticate("bob", "pw2"));
        assert!(users.authenticate("robert", "pw2"));
        assert!(users.drop_user("robert").unwrap());
        assert!(!users.drop_user("robert").unwrap());
        assert_eq!(Users::open(&path).unwrap().list().len(), 1);

        assert!(users.create("bad name", "x", &[], ExistingDatabase::Fail).is_err());
        assert!(users.create("ok", "", &[], ExistingDatabase::Fail).is_err());
        fs::write(&path, "nonsense\n").unwrap();
        assert!(Users::open(&path).is_err());
    }

    #[test]
    fn commands_check_access() {
        let users = Users::in_memory().with_iterations(10);
        let admin = Principal::User("root".into());
        let params = BTreeMap::from([("pw".to_string(), Value::String("s3cret".into()))]);
        let run_as = |who: &Principal, access, text: &str| match graphbuff::cypher::parse_statement(text).unwrap() {
            graphbuff::cypher::ast::Statement::User(cmd) => run(&users, who, access, &cmd, &params),
            _ => panic!("not a user command"),
        };
        run_as(&admin, Access::Admin, "CREATE USER root SET PASSWORD 'r'").unwrap();
        run_as(&admin, Access::Admin, "GRANT ROLE admin TO root").unwrap();
        let r = run_as(&admin, Access::Admin, "CREATE USER kim SET PASSWORD $pw").unwrap();
        assert_eq!(r.stats.system_updates, 1);
        assert!(users.authenticate("kim", "s3cret"));
        let r = run_as(&admin, Access::Admin, "GRANT ROLES reader, editor TO kim").unwrap();
        assert_eq!(r.stats.system_updates, 2);
        assert!(run_as(&admin, Access::Admin, "GRANT ROLE wizard TO kim").is_err());
        assert!(run_as(&admin, Access::Admin, "GRANT ROLE reader TO nobody").is_err());

        let kim = Principal::User("kim".into());
        assert!(matches!(
            run_as(&kim, Access::Write, "CREATE USER x SET PASSWORD 'x'"),
            Err(Error::Forbidden(_))
        ));
        let me = run_as(&kim, Access::Write, "SHOW CURRENT USER").unwrap();
        assert_eq!(me.rows[0][0], ResultValue::String("kim".into()));
        assert!(matches!(
            run_as(&kim, Access::Write, "ALTER CURRENT USER SET PASSWORD FROM 'wrong' TO 'n'"),
            Err(Error::Forbidden(_))
        ));
        run_as(&kim, Access::Write, "ALTER CURRENT USER SET PASSWORD FROM $pw TO 'n'").unwrap();
        assert!(users.authenticate("kim", "n"));

        let all = run_as(&admin, Access::Admin, "SHOW USERS").unwrap();
        assert_eq!(all.rows.len(), 2);
        assert!(run_as(&admin, Access::Admin, "DROP USER root").is_err());
        assert_eq!(run_as(&admin, Access::Admin, "DROP USER nobody IF EXISTS").unwrap().stats.system_updates, 0);
        run_as(&admin, Access::Admin, "REVOKE ROLE editor FROM kim").unwrap();
        assert_eq!(users.access("kim"), Some(Access::Read));
        assert!(matches!(
            run_as(&Principal::Token, Access::Admin, "ALTER CURRENT USER SET PASSWORD FROM 'a' TO 'b'"),
            Err(Error::Runtime(_))
        ));
    }
}
