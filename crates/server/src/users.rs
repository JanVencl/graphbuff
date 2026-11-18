//! User accounts: names, password hashes and roles, kept in the system database
//! ([`Users::system`]).
//!
//! In the system database each account is a node
//! `(:User {name, password, roles})` (`password` is the encoded hash, `roles` a list of
//! role names), with a uniqueness constraint on `name`. A `(:SystemInfo {version})` node
//! records the layout, so a newer layout is refused rather than misread. Every change
//! is one statement, so it is applied completely or not at all, and is synced to disk
//! before it takes effect.
//!
//! Passwords are stored as Argon2id hashes with a random 16-byte salt, in the PHC string
//! format (`$argon2id$v=19$m=65536,t=3,p=1$salt$hash`), so each hash carries its own
//! cost ([`HashParams::DEFAULT`] for new ones). A hash made with other parameters is
//! replaced at the user's next successful login. A successful login is remembered (as a
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
//! password. Once there is an administrator, no change may leave none: dropping the last
//! one, revoking their role or replacing their account is refused.
//!
//! New passwords need at least [`MIN_PASSWORD_LENGTH`] characters and must differ from
//! the user name.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use graphbuff::{BoxedStore, Database, Error, QueryResult, QueryStats, Result, ResultValue, Value};
use graphbuff::cypher::ast::{DbName, ExistingDatabase, Secret, UserCommand};

use crate::crypto::{self, Sha256};

/// The cost of Argon2id password hashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashParams {
    /// Memory in KiB (at least 8 per lane).
    pub memory_kib: u32,
    /// Passes over the memory.
    pub passes: u32,
    /// Lanes (degree of parallelism).
    pub lanes: u32,
}

impl HashParams {
    /// 64 MiB, 3 passes, 1 lane: OWASP's advice for Argon2id. About 0.1 s per hash.
    pub const DEFAULT: HashParams = HashParams { memory_kib: 64 * 1024, passes: 3, lanes: 1 };

    /// The cheapest Argon2id allows; for tests only.
    pub const TESTING: HashParams = HashParams { memory_kib: 8, passes: 1, lanes: 1 };

    fn argon2(self, out_len: usize) -> Option<argon2::Argon2<'static>> {
        let params = argon2::Params::new(self.memory_kib, self.passes, self.lanes, Some(out_len)).ok()?;
        Some(argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params))
    }
}

const SALT_BYTES: usize = 16;

/// The layout of the accounts in the system database that this version writes and reads.
const SYSTEM_VERSION: i64 = 1;

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

const HASH_BYTES: usize = 32;

/// The fewest characters a new password may have (NIST SP 800-63B's minimum).
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// An Argon2id password hash.
#[derive(Clone, PartialEq, Eq)]
struct PasswordHash {
    params: HashParams,
    salt: Vec<u8>,
    hash: Vec<u8>,
}

impl PasswordHash {
    fn new(password: &str, params: HashParams) -> Self {
        let salt = crypto::random_bytes()[..SALT_BYTES].to_vec();
        let mut hash = vec![0u8; HASH_BYTES];
        params
            .argon2(HASH_BYTES)
            .expect("valid hash parameters")
            .hash_password_into(password.as_bytes(), &salt, &mut hash)
            .expect("a 16-byte salt and a 32-byte hash are valid");
        PasswordHash { params, salt, hash }
    }

    fn matches(&self, password: &str) -> bool {
        let mut out = vec![0u8; self.hash.len()];
        let Some(argon2) = self.params.argon2(self.hash.len()) else {
            return false;
        };
        argon2.hash_password_into(password.as_bytes(), &self.salt, &mut out).is_ok()
            && crypto::same_bytes(&out, &self.hash)
    }

    /// A fast salted hash of `password`, for the cache of verified logins.
    fn quick(&self, password: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(&self.salt);
        h.update(&self.hash);
        h.update(password.as_bytes());
        h.finish()
    }

    /// The PHC string: `$argon2id$v=19$m=…,t=…,p=…$salt$hash` (unpadded base64).
    fn encode(&self) -> String {
        let HashParams { memory_kib, passes, lanes } = self.params;
        format!(
            "$argon2id$v=19$m={memory_kib},t={passes},p={lanes}${}${}",
            crypto::base64_encode(&self.salt),
            crypto::base64_encode(&self.hash)
        )
    }

    fn decode(s: &str) -> Option<Self> {
        let mut parts = s.split('$');
        let (Some(""), Some("argon2id"), Some("v=19"), Some(params), Some(salt), Some(hash), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        let mut values = params.split(',').map(|kv| kv.split_once('='));
        let (Some(Some(("m", m))), Some(Some(("t", t))), Some(Some(("p", p))), None) =
            (values.next(), values.next(), values.next(), values.next())
        else {
            return None;
        };
        let params = HashParams { memory_kib: m.parse().ok()?, passes: t.parse().ok()?, lanes: p.parse().ok()? };
        let (salt, hash) = (crypto::base64_decode(salt)?, crypto::base64_decode(hash)?);
        // Only what Argon2id accepts: salts of 8 bytes or more, hashes of 4 or more.
        if salt.len() < 8 || hash.len() < 4 || params.argon2(hash.len()).is_none() {
            return None;
        }
        // The canonical form only, so a stored hash always reads back the same.
        let hash = PasswordHash { params, salt, hash };
        (hash.encode() == s).then_some(hash)
    }
}

#[derive(Clone)]
struct Account {
    password: PasswordHash,
    roles: BTreeSet<Role>,
}

impl Account {
    fn is_admin(&self) -> bool {
        self.roles.contains(&Role::Admin)
    }
}

#[derive(Default)]
struct State {
    accounts: BTreeMap<String, Account>,
    /// Users whose password was checked: their [`PasswordHash::quick`] hash.
    verified: HashMap<String, [u8; 32]>,
}

/// Where the accounts are kept.
enum Store {
    /// Nowhere: they live as long as the [`Users`].
    Memory,
    /// The system database.
    System(Arc<Database<BoxedStore>>),
}

/// A set of user accounts. Changes are saved (see [`Store`]) before they take effect.
pub struct Users {
    store: Store,
    params: HashParams,
    min_password_length: usize,
    state: RwLock<State>,
}

impl fmt::Debug for Users {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let store = match &self.store {
            Store::Memory => "memory".to_string(),
            Store::System(_) => "system database".to_string(),
        };
        f.debug_struct("Users")
            .field("store", &store)
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

/// Checks a new password for `name`.
fn check_password(name: &str, password: &str, min_length: usize) -> Result<()> {
    if password.is_empty() {
        return Err(Error::Semantic("a password cannot be empty".into()));
    }
    if password.chars().count() < min_length {
        return Err(Error::Semantic(format!("a password needs at least {min_length} characters")));
    }
    if password == name {
        return Err(Error::Semantic("the password must differ from the user name".into()));
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
            store: Store::Memory,
            params: HashParams::DEFAULT,
            min_password_length: MIN_PASSWORD_LENGTH,
            state: RwLock::default(),
        }
    }

    /// The accounts kept in the system database `db`
    /// ([`graphbuff::DatabaseManager::system_database`]). Prepares an empty system
    /// database (constraint and layout version); refuses one with a newer layout.
    pub fn system(db: Arc<Database<BoxedStore>>) -> Result<Users> {
        let bad = |msg: String| Error::Storage(format!("system database: {msg}"));
        db.execute("CREATE CONSTRAINT system_user_name IF NOT EXISTS FOR (u:User) REQUIRE u.name IS UNIQUE")?;
        let info = db.execute_with_params(
            "MERGE (i:SystemInfo) ON CREATE SET i.version = $version RETURN i.version",
            &BTreeMap::from([("version".to_string(), Value::Int(SYSTEM_VERSION))]),
        )?;
        match info.rows.as_slice() {
            [row] => match row.first() {
                Some(ResultValue::Int(v)) if *v <= SYSTEM_VERSION => {}
                Some(ResultValue::Int(v)) => {
                    return Err(bad(format!(
                        "its accounts have layout version {v}, written by a newer graphbuff; this one reads up to {SYSTEM_VERSION}"
                    )));
                }
                other => return Err(bad(format!("invalid layout version {other:?}"))),
            },
            rows => return Err(bad(format!("{} SystemInfo nodes, expected one", rows.len()))),
        }

        let rows = db.execute("MATCH (u:User) RETURN u.name, u.password, u.roles")?.rows;
        let mut accounts = BTreeMap::new();
        for row in rows {
            let [ResultValue::String(name), ResultValue::String(hash), roles] = row.as_slice() else {
                return Err(bad(format!("invalid user record {row:?}")));
            };
            check_name(name).map_err(|_| bad(format!("invalid user name `{name}`")))?;
            let password = PasswordHash::decode(hash).ok_or_else(|| bad(format!("invalid password hash of `{name}`")))?;
            let roles = match roles {
                ResultValue::List(list) => list
                    .iter()
                    .map(|r| match r {
                        ResultValue::String(r) => Role::parse(r).map_err(|_| bad(format!("unknown role `{r}` of `{name}`"))),
                        other => Err(bad(format!("invalid role {other:?} of `{name}`"))),
                    })
                    .collect::<Result<BTreeSet<Role>>>()?,
                ResultValue::Null => BTreeSet::new(),
                other => return Err(bad(format!("invalid roles {other:?} of `{name}`"))),
            };
            if accounts.insert(name.clone(), Account { password, roles }).is_some() {
                return Err(bad(format!("duplicate user `{name}`")));
            }
        }
        Ok(Users {
            store: Store::System(db),
            params: HashParams::DEFAULT,
            min_password_length: MIN_PASSWORD_LENGTH,
            state: RwLock::new(State {
                accounts,
                verified: HashMap::new(),
            }),
        })
    }

    /// Hashes new passwords with `params` instead of [`HashParams::DEFAULT`] (cheaper
    /// ones only for tests). Hashes made with other parameters are replaced at the next
    /// successful login.
    pub fn with_hash_params(mut self, params: HashParams) -> Users {
        self.params = params;
        self
    }

    /// Lets new passwords be shorter than [`MIN_PASSWORD_LENGTH`] (at least 1); for tests.
    pub fn with_min_password_length(mut self, min: usize) -> Users {
        self.min_password_length = min.max(1);
        self
    }

    /// Whether some user holds the admin role.
    pub fn has_admin(&self) -> bool {
        self.read().accounts.values().any(Account::is_admin)
    }

    /// Whether the accounts are kept in the system database.
    pub fn in_system_database(&self) -> bool {
        matches!(self.store, Store::System(_))
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

    /// Whether `password` is `name`'s password. A hash made with parameters other than
    /// this set's is replaced by a new one (best effort).
    pub fn authenticate(&self, name: &str, password: &str) -> bool {
        let stored = {
            let st = self.read();
            let Some(account) = st.accounts.get(name) else {
                return false;
            };
            if let Some(quick) = st.verified.get(name) {
                return crypto::same_bytes(quick, &account.password.quick(password));
            }
            account.password.clone()
        };
        // The slow hashes run without the lock.
        if !stored.matches(password) {
            return false;
        }
        let mut current = stored.clone();
        if stored.params != self.params {
            let new = PasswordHash::new(password, self.params);
            let upgraded = self.change(|accounts| match accounts.get_mut(name) {
                // Only if the password did not change meanwhile.
                Some(a) if a.password == stored => {
                    a.password = new.clone();
                    Ok(true)
                }
                _ => Ok(false),
            });
            if let Ok(true) = upgraded {
                current = new;
            }
        }
        let mut st = self.write();
        // Remember it only if the password did not change meanwhile.
        if st.accounts.get(name).is_some_and(|a| a.password == current) {
            st.verified.insert(name.to_string(), current.quick(password));
        }
        true
    }

    /// Applies `change` to a copy of the accounts, saves the copy, then makes it
    /// current. Returns what `change` returned.
    fn change<T>(&self, change: impl FnOnce(&mut BTreeMap<String, Account>) -> Result<T>) -> Result<T> {
        let mut st = self.write();
        let mut accounts = st.accounts.clone();
        let out = change(&mut accounts)?;
        if st.accounts.values().any(Account::is_admin) && !accounts.values().any(Account::is_admin) {
            return Err(Error::Constraint(
                "this would leave no administrator; give another user the admin role first".into(),
            ));
        }
        match &self.store {
            Store::Memory => {}
            Store::System(db) => save_system(db, &st.accounts, &accounts)?,
        }
        let State { accounts: old, verified } = &mut *st;
        verified.retain(|name, _| {
            matches!((old.get(name), accounts.get(name)), (Some(a), Some(b)) if a.password == b.password)
        });
        st.accounts = accounts;
        Ok(out)
    }

    /// Creates a user, or replaces an existing one when `replace` is set. Returns
    /// whether anything changed.
    pub fn create(&self, name: &str, password: &str, roles: &[Role], existing: ExistingDatabase) -> Result<bool> {
        check_name(name)?;
        check_password(name, password, self.min_password_length)?;
        if existing == ExistingDatabase::Keep && self.read().accounts.contains_key(name) {
            return Ok(false);
        }
        let password = PasswordHash::new(password, self.params);
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
        check_password(name, password, self.min_password_length)?;
        if !self.read().accounts.contains_key(name) {
            return Err(no_user(name));
        }
        let password = PasswordHash::new(password, self.params);
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

/// Writes the difference between `old` and `new` to the system database, in one
/// statement: accounts that are gone or renamed are deleted, new and changed ones are
/// written.
fn save_system(db: &Database<BoxedStore>, old: &BTreeMap<String, Account>, new: &BTreeMap<String, Account>) -> Result<()> {
    let remove: Vec<Value> = old
        .keys()
        .filter(|name| !new.contains_key(*name))
        .map(|name| Value::String(name.clone()))
        .collect();
    let upsert: Vec<Value> = new
        .iter()
        .filter(|(name, a)| {
            old.get(*name)
                .is_none_or(|o| o.roles != a.roles || o.password != a.password)
        })
        .map(|(name, a)| {
            Value::Map(BTreeMap::from([
                ("name".to_string(), Value::String(name.clone())),
                ("password".to_string(), Value::String(a.password.encode())),
                (
                    "roles".to_string(),
                    Value::List(a.roles.iter().map(|r| Value::String(r.name().into())).collect()),
                ),
            ]))
        })
        .collect();
    if remove.is_empty() && upsert.is_empty() {
        return Ok(());
    }
    db.execute_with_params(
        "OPTIONAL MATCH (old:User) WHERE old.name IN $remove \
         DELETE old \
         WITH count(*) AS ignored \
         UNWIND $upsert AS a \
         MERGE (u:User {name: a.name}) \
         SET u.password = a.password, u.roles = a.roles",
        &BTreeMap::from([
            ("remove".to_string(), Value::List(remove)),
            ("upsert".to_string(), Value::List(upsert)),
        ]),
    )
    .map_err(|e| Error::Storage(format!("saving the accounts in the system database: {e}")))?;
    Ok(())
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

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!("graphbuff-users-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn password_policy() {
        let users = Users::in_memory().with_hash_params(HashParams::TESTING);
        let err = users.create("ann", "short", &[], ExistingDatabase::Fail).unwrap_err();
        assert!(err.to_string().contains("at least 8 characters"), "{err}");
        let err = users.create("annabelle", "annabelle", &[], ExistingDatabase::Fail).unwrap_err();
        assert!(err.to_string().contains("differ from the user name"), "{err}");
        // Characters, not bytes.
        assert!(users.create("ann", "ééééééé", &[], ExistingDatabase::Fail).is_err());
        users.create("ann", "éééééééé", &[], ExistingDatabase::Fail).unwrap();
        assert!(users.set_password("ann", "1234567").is_err());
        users.set_password("ann", "12345678").unwrap();
        assert!(users.authenticate("ann", "12345678"));
    }

    #[test]
    fn the_last_administrator_stays() {
        let users = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
        // Without an administrator, anything goes.
        users.create("kim", "k", &[Role::Reader], ExistingDatabase::Fail).unwrap();
        assert!(!users.has_admin());
        users.create("root", "r", &[Role::Admin], ExistingDatabase::Fail).unwrap();
        assert!(users.has_admin());
        let refused = |r: Result<bool>| match r {
            Err(Error::Constraint(m)) => assert!(m.contains("no administrator"), "{m}"),
            other => panic!("{other:?}"),
        };
        refused(users.drop_user("root"));
        refused(users.revoke("root", Role::Admin));
        refused(users.create("root", "r2", &[], ExistingDatabase::Replace));
        assert!(users.has_admin() && users.authenticate("root", "r"));
        // With a second administrator, the first may go.
        users.grant("kim", Role::Admin).unwrap();
        assert!(users.revoke("root", Role::Admin).unwrap());
        assert!(users.rename("kim", "boss").unwrap());
        refused(users.drop_user("boss"));
        assert!(users.drop_user("root").unwrap());
    }

    #[test]
    fn logins_and_their_cache() {
        let users = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
        assert!(users.create("alice", "pw1", &[Role::Admin], ExistingDatabase::Fail).unwrap());
        assert!(users.create("bob", "x", &[], ExistingDatabase::Fail).unwrap());
        assert!(users.create("bob", "y", &[], ExistingDatabase::Fail).is_err());
        assert!(!users.create("bob", "y", &[], ExistingDatabase::Keep).unwrap());
        assert!(users.grant("bob", Role::Reader).unwrap());
        assert!(!users.grant("bob", Role::Reader).unwrap());
        assert!(users.authenticate("alice", "pw1"));
        assert!(users.authenticate("alice", "pw1")); // from the cache
        assert!(!users.authenticate("alice", "pw2"));
        assert!(!users.authenticate("carol", "pw1"));
        assert_eq!(users.access("bob"), Some(Access::Read));
        assert_eq!(users.access("nobody"), None);
        // A new password replaces the remembered login; so does a rename.
        users.set_password("alice", "new").unwrap();
        assert!(!users.authenticate("alice", "pw1"));
        assert!(users.authenticate("alice", "new"));
        assert!(users.authenticate("bob", "x"));
        assert!(users.rename("bob", "robert").unwrap());
        assert!(!users.authenticate("bob", "x"));
        assert!(users.authenticate("robert", "x"));
        assert!(users.create("bad name", "x", &[], ExistingDatabase::Fail).is_err());
        assert!(users.create("ok", "", &[], ExistingDatabase::Fail).is_err());
    }

    #[test]
    fn hashes_are_argon2id_phc_strings() {
        let h = PasswordHash::new("pw", HashParams::TESTING);
        let text = h.encode();
        assert!(text.starts_with("$argon2id$v=19$m=8,t=1,p=1$"), "{text}");
        // 16-byte salt and 32-byte hash, unpadded base64.
        let parts: Vec<&str> = text.split('$').collect();
        assert_eq!((parts[4].len(), parts[5].len()), (22, 43), "{text}");
        let back = PasswordHash::decode(&text).unwrap();
        assert!(back == h && back.matches("pw") && !back.matches("pW"));
        // Salted: the same password hashes differently.
        assert_ne!(PasswordHash::new("pw", HashParams::TESTING).encode(), text);

        let default = PasswordHash::new("pw", HashParams::DEFAULT);
        assert!(default.encode().starts_with("$argon2id$v=19$m=65536,t=3,p=1$"));
        assert!(default.matches("pw"));

        // Made by the reference implementation (argon2-cffi): both directions agree.
        for (password, phc) in [
            ("correct horse", "$argon2id$v=19$m=64,t=2,p=1$Z3JhcGhidWZmLXNhbHQtMQ$YKk+RR9y9LcNC5eIJtuY7yW+MxGWv2nMgMfZnbMyPXg"),
            ("pw", "$argon2id$v=19$m=65536,t=3,p=1$MDEyMzQ1Njc4OWFiY2RlZg$Gs6vDcp/gVcHT06DjWRf+cpRnCdZDQMnY77kwsQjXjo"),
        ] {
            let h = PasswordHash::decode(phc).unwrap();
            assert!(h.matches(password) && !h.matches("x"));
            assert_eq!(h.encode(), phc);
        }

        let (salt, hash) = (parts[4], parts[5]);
        for bad in [
            String::new(),
            "pbkdf2-sha256:600000:00:00".to_string(),
            format!("$argon2i$v=19$m=8,t=1,p=1${salt}${hash}"),
            format!("$argon2id$v=16$m=8,t=1,p=1${salt}${hash}"),
            format!("$argon2id$v=19$t=1,m=8,p=1${salt}${hash}"),
            format!("$argon2id$v=19$m=4,t=1,p=1${salt}${hash}"),
            format!("$argon2id$v=19$m=8,t=0,p=1${salt}${hash}"),
            format!("$argon2id$v=19$m=8,t=1,p=1$c2FsdA${hash}"),
            format!("$argon2id$v=19$m=8,t=1,p=1${salt}${hash}="),
            format!("$argon2id$v=19$m=8,t=1,p=1${salt}${hash}$"),
            format!("$argon2id$v=19$m=08,t=1,p=1${salt}${hash}"),
        ] {
            assert!(PasswordHash::decode(&bad).is_none(), "{bad} accepted");
        }
    }

    #[test]
    fn hashes_with_other_parameters_are_replaced_at_login() {
        let mgr = graphbuff::DatabaseManager::in_memory();
        let system = mgr.system_database().unwrap();
        let cheap = HashParams { memory_kib: 16, ..HashParams::TESTING };
        let stored = |name: &str| {
            let r = system
                .execute_with_params(
                    "MATCH (u:User {name: $n}) RETURN u.password",
                    &BTreeMap::from([("n".to_string(), Value::String(name.into()))]),
                )
                .unwrap();
            match &r.rows[0][0] {
                ResultValue::String(s) => s.clone(),
                other => panic!("{other:?}"),
            }
        };
        let old = Users::system(Arc::clone(&system)).unwrap().with_hash_params(cheap).with_min_password_length(1);
        old.create("ann", "pw", &[], ExistingDatabase::Fail).unwrap();
        assert!(stored("ann").contains("$m=16,"));

        let users = Users::system(Arc::clone(&system)).unwrap().with_hash_params(HashParams::TESTING).with_min_password_length(1);
        assert!(!users.authenticate("ann", "wrong"));
        assert!(stored("ann").contains("$m=16,"), "a failed login changes nothing");
        assert!(users.authenticate("ann", "pw"));
        assert!(stored("ann").contains("$m=8,"), "{}", stored("ann"));
        assert!(users.authenticate("ann", "pw"));
        assert!(!users.authenticate("ann", "wrong"));
        // Saved: a fresh reader sees the new hash.
        let again = Users::system(Arc::clone(&system)).unwrap().with_hash_params(HashParams::TESTING).with_min_password_length(1);
        assert!(again.authenticate("ann", "pw"));
    }

    #[test]
    fn accounts_persist_in_the_system_database() {
        let tmp = TempDir::new("system");
        let dir = &tmp.0;
        {
            let mgr = graphbuff::DatabaseManager::open(dir).unwrap();
            let users = Users::system(mgr.system_database().unwrap()).unwrap().with_hash_params(HashParams::TESTING).with_min_password_length(1);
            assert!(users.in_system_database());
            assert!(users.is_empty());
            users.create("alice", "pw1", &[Role::Admin], ExistingDatabase::Fail).unwrap();
            users.create("bob", "pw2", &[Role::Reader, Role::Editor], ExistingDatabase::Fail).unwrap();
            users.create("carol", "pw3", &[], ExistingDatabase::Fail).unwrap();
            assert!(users.revoke("bob", Role::Editor).unwrap());
            users.set_password("carol", "new").unwrap();
            assert!(users.rename("bob", "robert").unwrap());
            assert!(users.create("dave", "pw4", &[], ExistingDatabase::Fail).unwrap());
            assert!(users.drop_user("dave").unwrap());
            // Failing changes leave nothing behind.
            assert!(users.rename("robert", "alice").is_err());
            assert!(users.grant("nobody", Role::Admin).is_err());
        }
        let mgr = graphbuff::DatabaseManager::open(dir).unwrap();
        let system = mgr.system_database().unwrap();
        let users = Users::system(Arc::clone(&system)).unwrap();
        assert_eq!(
            users.list(),
            vec![
                ("alice".into(), vec![Role::Admin]),
                ("carol".into(), vec![]),
                ("robert".into(), vec![Role::Reader]),
            ]
        );
        assert!(users.authenticate("alice", "pw1"));
        assert!(users.authenticate("robert", "pw2"));
        assert!(users.authenticate("carol", "new"));
        assert!(!users.authenticate("carol", "pw3"));
        // Opening again does not add a second constraint or SystemInfo.
        drop(users);
        Users::system(Arc::clone(&system)).unwrap();
        let info = system.execute("MATCH (i:SystemInfo) RETURN i.version").unwrap();
        assert_eq!(info.rows, vec![vec![ResultValue::Int(SYSTEM_VERSION)]]);
        // The constraint holds even for writes that bypass Users.
        assert!(system.execute("CREATE (:User {name: 'alice'})").is_err());
    }

    #[test]
    fn a_newer_system_layout_is_refused() {
        let mgr = graphbuff::DatabaseManager::in_memory();
        let system = mgr.system_database().unwrap();
        Users::system(Arc::clone(&system)).unwrap();
        system.execute("MATCH (i:SystemInfo) SET i.version = 99").unwrap();
        let err = Users::system(Arc::clone(&system)).unwrap_err();
        assert!(err.to_string().contains("newer graphbuff"), "{err}");
        system.execute("MATCH (i:SystemInfo) SET i.version = 1 CREATE (:User {name: 'x', password: 'junk'})").unwrap();
        assert!(Users::system(system).is_err());
    }

    #[test]
    fn commands_check_access() {
        let users = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
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
