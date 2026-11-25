//! User accounts: names, password hashes and roles, kept in the system database
//! ([`Users::system`]).
//!
//! In the system database each account is a node
//! `(:User {name, password, roles, databaseRoles})` (`password` is the encoded hash,
//! `roles` a list of role names, `databaseRoles` a list of `role:database` strings), with
//! a uniqueness constraint on `name`. A `(:SystemInfo {version})` node
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
//! Each user has a set of roles, held everywhere or (reader and editor) on one database
//! only; on each database, the strongest role the user holds there decides what they may
//! do:
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
//!
//! A user may also have API tokens, for programs: `gbt_<id>_<secret>`, where the secret is
//! 256 random bits, stored as a SHA-256 hash (a random secret needs no slow hash). A token
//! may do what its user may, but only up to its own cap (reading, or also writing) and
//! only on its databases, if it names some; it never administers, nor manages tokens or
//! passwords. In the system database each token is a node
//! `(:ApiToken {id, hash, name, user, accountId, access, databases, created, expires,
//! lastUsed})`. It belongs to the account whose random `accountId` it has, not to whoever
//! has the name: a user who is dropped or replaced loses their tokens. Tokens are not
//! connected to the `User` nodes, so that versions without tokens can still delete those.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graphbuff::{BoxedStore, Database, Error, QueryResult, QueryStats, Result, ResultValue, Temporal, Value};
use graphbuff::cypher::ast::{DbName, ExistingDatabase, Secret, TokenExpiry, UserCommand};

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

/// How long an API token works unless it is made with another expiry.
pub const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(90 * 24 * 3600);

/// The most API tokens one user may have.
pub const MAX_TOKENS: usize = 100;

/// How often the time a token was last used is saved, in milliseconds.
const SAVE_LAST_USED: i64 = 3_600_000;

const TOKEN_PREFIX: &str = "gbt_";

/// What an API token may do at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenAccess {
    Read,
    /// Also write data, indexes and constraints.
    Write,
}

impl TokenAccess {
    pub fn name(self) -> &'static str {
        match self {
            TokenAccess::Read => "read",
            TokenAccess::Write => "write",
        }
    }

    /// Parses `read` or `write` (case-insensitive).
    pub fn parse(name: &str) -> Result<TokenAccess> {
        [TokenAccess::Read, TokenAccess::Write]
            .into_iter()
            .find(|a| a.name().eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::Runtime(format!("a token's access is read or write, not `{name}`")))
    }

    fn access(self) -> Access {
        match self {
            TokenAccess::Read => Access::Read,
            TokenAccess::Write => Access::Write,
        }
    }
}

/// An API token of a user, without its secret. Times are milliseconds since 1970.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    /// The public part: the token reads `gbt_<id>_<secret>`.
    pub id: String,
    pub name: String,
    pub user: String,
    pub access: TokenAccess,
    /// The databases it works on (lowercase); empty for every one.
    pub databases: BTreeSet<String>,
    pub created: i64,
    /// When it stops working, if ever.
    pub expires: Option<i64>,
    pub last_used: Option<i64>,
}

impl TokenInfo {
    fn expired(&self, now: i64) -> bool {
        self.expires.is_some_and(|e| e <= now)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ApiToken {
    info: TokenInfo,
    /// The SHA-256 hash of the whole token.
    hash: [u8; 32],
    /// The [`Account::id`] it belongs to.
    account: String,
}

/// The id of a well-formed API token (whether it exists or not); `None` for other text.
/// The id is not secret.
pub(crate) fn token_id(token: &str) -> Option<&str> {
    let (id, secret) = token.strip_prefix(TOKEN_PREFIX)?.split_once('_')?;
    crypto::unhex::<8>(id)?;
    crypto::unhex::<32>(secret)?;
    Some(id)
}

fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn random_hex<const N: usize>() -> Result<String> {
    let bytes = crypto::secret_bytes::<N>().map_err(|e| Error::Storage(format!("no random numbers: {e}")))?;
    Ok(crypto::hex(&bytes))
}

#[derive(Clone)]
struct Account {
    /// A random id, given when the account gets its first API token.
    id: Option<String>,
    password: PasswordHash,
    /// Roles held on every database.
    roles: BTreeSet<Role>,
    /// Roles held on one database (by lowercase name) only.
    databases: BTreeMap<String, BTreeSet<Role>>,
}

/// The key of database `name` in [`Account::databases`].
fn db_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

impl Account {
    fn is_admin(&self) -> bool {
        self.roles.contains(&Role::Admin)
    }

    /// What the account may do on every database.
    fn access(&self) -> Access {
        self.roles.iter().map(|r| r.access()).max().unwrap_or(Access::None)
    }

    /// What it may do on database `db`.
    fn access_on(&self, db: &str) -> Access {
        let here = self.databases.get(&db_key(db)).into_iter().flatten();
        self.roles.iter().chain(here).map(|r| r.access()).max().unwrap_or(Access::None)
    }

    /// The most it may do on some database.
    fn best_access(&self) -> Access {
        let all = self.roles.iter().chain(self.databases.values().flatten());
        all.map(|r| r.access()).max().unwrap_or(Access::None)
    }
}

/// What is saved.
#[derive(Clone, Default)]
struct Data {
    accounts: BTreeMap<String, Account>,
    /// API tokens by id. Each belongs to an account that exists.
    tokens: BTreeMap<String, ApiToken>,
}

impl Data {
    /// Drops the tokens whose account is gone or was replaced.
    fn drop_orphans(&mut self) {
        let accounts = &self.accounts;
        self.tokens.retain(|_, t| {
            accounts.get(&t.info.user).and_then(|a| a.id.as_ref()).is_some_and(|id| *id == t.account)
        });
    }
}

#[derive(Default)]
struct State {
    data: Data,
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
    /// Limits the password hashes computed at once (each takes the hash's memory).
    hashing: Gate,
    /// When each API token was last used, by id; saved now and then.
    used: Mutex<HashMap<String, i64>>,
}

/// A counting semaphore: at most `max` holders at once, the others wait.
struct Gate {
    held: Mutex<usize>,
    freed: Condvar,
    max: usize,
}

impl Default for Gate {
    /// One holder per CPU (at least two).
    fn default() -> Gate {
        let max = std::thread::available_parallelism().map_or(2, |n| n.get().max(2));
        Gate { held: Mutex::new(0), freed: Condvar::new(), max }
    }
}

impl Gate {
    /// Runs `f` once fewer than `max` others are running.
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Release<'a>(&'a Gate);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                *self.0.held.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                self.0.freed.notify_one();
            }
        }
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while *held >= self.max {
            held = self.freed.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        *held += 1;
        drop(held);
        let _release = Release(self);
        f()
    }
}

impl fmt::Debug for Users {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let store = match &self.store {
            Store::Memory => "memory".to_string(),
            Store::System(_) => "system database".to_string(),
        };
        f.debug_struct("Users")
            .field("store", &store)
            .field("users", &self.read().data.accounts.keys().collect::<Vec<_>>())
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

fn check_token_name(name: &str) -> Result<()> {
    check_name(name).map_err(|_| {
        Error::Semantic(format!("invalid token name `{name}`: use 1 to 64 letters, digits, '_', '.', '@' and '-'"))
    })
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
            hashing: Gate::default(),
            used: Mutex::default(),
        }
    }

    /// The accounts kept in the system database `db`
    /// ([`graphbuff::DatabaseManager::system_database`]). Prepares an empty system
    /// database (constraint and layout version); refuses one with a newer layout.
    pub fn system(db: Arc<Database<BoxedStore>>) -> Result<Users> {
        let bad = |msg: String| Error::Storage(format!("system database: {msg}"));
        db.execute("CREATE CONSTRAINT system_user_name IF NOT EXISTS FOR (u:User) REQUIRE u.name IS UNIQUE")?;
        db.execute("CREATE CONSTRAINT system_api_token_id IF NOT EXISTS FOR (t:ApiToken) REQUIRE t.id IS UNIQUE")?;
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

        let rows = db.execute("MATCH (u:User) RETURN u.name, u.password, u.roles, u.databaseRoles, u.accountId")?.rows;
        let mut accounts = BTreeMap::new();
        for row in rows {
            let [ResultValue::String(name), ResultValue::String(hash), roles, db_roles, id] = row.as_slice() else {
                return Err(bad(format!("invalid user record {row:?}")));
            };
            let id = match id {
                ResultValue::Null => None,
                ResultValue::String(id) => Some(id.clone()),
                other => return Err(bad(format!("invalid account id {other:?} of `{name}`"))),
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
            let mut databases: BTreeMap<String, BTreeSet<Role>> = BTreeMap::new();
            match db_roles {
                ResultValue::Null => {}
                ResultValue::List(list) => {
                    for r in list {
                        let parsed = match r {
                            ResultValue::String(s) => s
                                .split_once(':')
                                .and_then(|(role, db)| Some((Role::parse(role).ok()?, db)))
                                .filter(|(role, db)| *role != Role::Admin && !db.is_empty()),
                            _ => None,
                        };
                        let Some((role, db)) = parsed else {
                            return Err(bad(format!("invalid database role {r:?} of `{name}`")));
                        };
                        databases.entry(db_key(db)).or_default().insert(role);
                    }
                }
                other => return Err(bad(format!("invalid database roles {other:?} of `{name}`"))),
            }
            if accounts.insert(name.clone(), Account { id, password, roles, databases }).is_some() {
                return Err(bad(format!("duplicate user `{name}`")));
            }
        }
        let mut data = Data { accounts, tokens: load_tokens(&db)? };
        data.drop_orphans();
        Ok(Users {
            store: Store::System(db),
            params: HashParams::DEFAULT,
            min_password_length: MIN_PASSWORD_LENGTH,
            state: RwLock::new(State {
                data,
                verified: HashMap::new(),
            }),
            hashing: Gate::default(),
            used: Mutex::default(),
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
        self.read().data.accounts.values().any(Account::is_admin)
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
        self.read().data.accounts.is_empty()
    }

    /// Every user with their roles, by name.
    pub fn list(&self) -> Vec<(String, Vec<Role>)> {
        self.read()
            .data
            .accounts
            .iter()
            .map(|(n, a)| (n.clone(), a.roles.iter().copied().collect()))
            .collect()
    }

    /// The roles of `name`, if the user exists.
    pub fn roles(&self, name: &str) -> Option<Vec<Role>> {
        self.read().data.accounts.get(name).map(|a| a.roles.iter().copied().collect())
    }

    /// The roles `name` holds on database `db` only, by database, if the user exists.
    pub fn database_roles(&self, name: &str) -> Option<BTreeMap<String, Vec<Role>>> {
        let st = self.read();
        let a = st.data.accounts.get(name)?;
        Some(a.databases.iter().map(|(db, r)| (db.clone(), r.iter().copied().collect())).collect())
    }

    /// What `name` may do everywhere (its roles held on every database); `None` if there
    /// is no such user (any more).
    pub(crate) fn access(&self, name: &str) -> Option<Access> {
        let st = self.read();
        let account = st.data.accounts.get(name)?;
        Some(account.access())
    }

    /// What `name` may do on database `db`: its strongest role held everywhere or there.
    pub(crate) fn access_on(&self, name: &str, db: &str) -> Option<Access> {
        let st = self.read();
        let account = st.data.accounts.get(name)?;
        Some(account.access_on(db))
    }

    /// The most `name` may do on some database.
    pub(crate) fn best_access(&self, name: &str) -> Option<Access> {
        let st = self.read();
        let account = st.data.accounts.get(name)?;
        Some(account.best_access())
    }

    // ---- API tokens ----

    /// Who the API token `token` belongs to: its principal, also if it has expired (it
    /// may then do nothing, see [`Users::token_access`]), or `None` if there is no such
    /// token.
    pub(crate) fn authenticate_token(&self, token: &str) -> Option<Principal> {
        let id = token_id(token)?;
        let now = now_millis();
        let (who, saved) = {
            let st = self.read();
            let t = st.data.tokens.get(id)?;
            if !crypto::same_bytes(&crypto::sha256(token.as_bytes()), &t.hash) {
                return None;
            }
            let who = Principal::ApiToken { user: t.info.user.clone(), id: id.to_string() };
            if t.info.expired(now) {
                return Some(who);
            }
            (who, t.info.last_used)
        };
        self.used.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), now);
        if saved.is_none_or(|s| now - s >= SAVE_LAST_USED) {
            // Only for information: a login does not fail because this could not be saved.
            let _ = self.change(|d| {
                if let Some(t) = d.tokens.get_mut(id) {
                    t.info.last_used = Some(now);
                }
                Ok(())
            });
        }
        Some(who)
    }

    /// What `pick` says of a token that has not expired and its account: `None` for no
    /// such token (any more).
    fn token<T>(&self, id: &str, pick: impl FnOnce(&TokenInfo, &Account) -> T) -> Option<T> {
        let st = self.read();
        let t = st.data.tokens.get(id).filter(|t| !t.info.expired(now_millis()))?;
        let account = st.data.accounts.get(&t.info.user)?;
        Some(pick(&t.info, account))
    }

    /// What API token `id` may do everywhere: nothing if it is limited to some databases.
    pub(crate) fn token_access(&self, id: &str) -> Option<Access> {
        self.token(id, |t, account| match t.databases.is_empty() {
            true => account.access().min(t.access.access()),
            false => Access::None,
        })
    }

    /// What API token `id` may do on database `db`.
    pub(crate) fn token_access_on(&self, id: &str, db: &str) -> Option<Access> {
        self.token(id, |t, account| match t.databases.is_empty() || t.databases.contains(&db_key(db)) {
            true => account.access_on(db).min(t.access.access()),
            false => Access::None,
        })
    }

    /// The most API token `id` may do on some database.
    pub(crate) fn token_best_access(&self, id: &str) -> Option<Access> {
        self.token(id, |t, account| {
            let most = match t.databases.is_empty() {
                true => account.best_access(),
                false => t.databases.iter().map(|db| account.access_on(db)).max().unwrap_or(Access::None),
            };
            most.min(t.access.access())
        })
    }

    /// Makes an API token of `user` and returns it: the only time its secret is known.
    /// It may do what `user` may, up to `access`, on `databases` (every one if empty), for
    /// `lifetime` (`None`: until it is dropped).
    pub fn create_token(
        &self,
        user: &str,
        name: &str,
        access: TokenAccess,
        databases: &[String],
        lifetime: Option<Duration>,
    ) -> Result<(String, TokenInfo)> {
        check_token_name(name)?;
        let id = random_hex::<8>()?;
        let token = format!("{TOKEN_PREFIX}{id}_{}", random_hex::<32>()?);
        let new_account = random_hex::<16>()?;
        let created = now_millis();
        let info = TokenInfo {
            id: id.clone(),
            name: name.to_string(),
            user: user.to_string(),
            access,
            databases: databases.iter().map(|d| db_key(d)).collect(),
            created,
            expires: lifetime.map(|l| created.saturating_add(i64::try_from(l.as_millis()).unwrap_or(i64::MAX))),
            last_used: None,
        };
        let hash = crypto::sha256(token.as_bytes());
        self.change(|d| {
            let account = d.accounts.get_mut(user).ok_or_else(|| no_user(user))?;
            let of_user = || d.tokens.values().filter(|t| t.info.user == user);
            if of_user().any(|t| t.info.name == name) {
                return Err(Error::Runtime(format!("user `{user}` already has a token `{name}`")));
            }
            if of_user().count() >= MAX_TOKENS {
                return Err(Error::Runtime(format!("user `{user}` already has {MAX_TOKENS} tokens; drop some first")));
            }
            let account = account.id.get_or_insert(new_account).clone();
            d.tokens.insert(id, ApiToken { info: info.clone(), hash, account });
            Ok(())
        })?;
        Ok((token, info))
    }

    /// The API tokens of `user` (`None`: of every user), by user and name.
    pub fn tokens(&self, user: Option<&str>) -> Vec<TokenInfo> {
        let mut list: Vec<TokenInfo> = {
            let st = self.read();
            let wanted = st.data.tokens.values().filter(|t| user.is_none_or(|u| t.info.user == u));
            wanted.map(|t| t.info.clone()).collect()
        };
        let used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        for t in &mut list {
            t.last_used = t.last_used.max(used.get(&t.id).copied());
        }
        list.sort_by(|a, b| (&a.user, &a.name).cmp(&(&b.user, &b.name)));
        list
    }

    /// Removes the API token `name` of `user`; returns whether there was one.
    pub fn drop_token(&self, user: &str, name: &str) -> Result<bool> {
        let id = {
            let st = self.read();
            if !st.data.accounts.contains_key(user) {
                return Err(no_user(user));
            }
            match st.data.tokens.values().find(|t| t.info.user == user && t.info.name == name) {
                Some(t) => t.info.id.clone(),
                None => return Ok(false),
            }
        };
        self.change(|d| Ok(d.tokens.remove(&id).is_some()))
    }

    /// Whether `password` is `name`'s password. A hash made with parameters other than
    /// this set's is replaced by a new one (best effort).
    pub fn authenticate(&self, name: &str, password: &str) -> bool {
        let stored = {
            let st = self.read();
            let Some(account) = st.data.accounts.get(name) else {
                return false;
            };
            if let Some(quick) = st.verified.get(name) {
                return crypto::same_bytes(quick, &account.password.quick(password));
            }
            account.password.clone()
        };
        // The slow hashes run without the lock, a few at a time: each takes the hash's
        // memory (64 MiB by default), and logins must not exhaust it.
        if !self.hashing.run(|| stored.matches(password)) {
            return false;
        }
        let mut current = stored.clone();
        if stored.params != self.params {
            let new = self.hashing.run(|| PasswordHash::new(password, self.params));
            let upgraded = self.change(|d| match d.accounts.get_mut(name) {
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
        if st.data.accounts.get(name).is_some_and(|a| a.password == current) {
            st.verified.insert(name.to_string(), current.quick(password));
        }
        true
    }

    /// Applies `change` to a copy of the accounts and tokens, drops the tokens of accounts
    /// that are gone, saves the copy, then makes it current. Returns what `change`
    /// returned.
    fn change<T>(&self, change: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        let mut st = self.write();
        let mut data = st.data.clone();
        let out = change(&mut data)?;
        data.drop_orphans();
        let has_admin = |d: &Data| d.accounts.values().any(Account::is_admin);
        if has_admin(&st.data) && !has_admin(&data) {
            return Err(Error::Constraint(
                "this would leave no administrator; give another user the admin role first".into(),
            ));
        }
        match &self.store {
            Store::Memory => {}
            Store::System(db) => save_system(db, &st.data, &data)?,
        }
        let State { data: old, verified } = &mut *st;
        verified.retain(|name, _| {
            matches!((old.accounts.get(name), data.accounts.get(name)), (Some(a), Some(b)) if a.password == b.password)
        });
        st.data = data;
        Ok(out)
    }

    /// Creates a user, or replaces an existing one when `replace` is set. Returns
    /// whether anything changed.
    pub fn create(&self, name: &str, password: &str, roles: &[Role], existing: ExistingDatabase) -> Result<bool> {
        check_name(name)?;
        check_password(name, password, self.min_password_length)?;
        if existing == ExistingDatabase::Keep && self.read().data.accounts.contains_key(name) {
            return Ok(false);
        }
        let password = PasswordHash::new(password, self.params);
        self.change(|Data { accounts, .. }| {
            if accounts.contains_key(name) {
                match existing {
                    ExistingDatabase::Fail => return Err(Error::Runtime(format!("user `{name}` already exists"))),
                    ExistingDatabase::Keep => return Ok(false),
                    ExistingDatabase::Replace => {}
                }
            }
            let roles = roles.iter().copied().collect();
            accounts.insert(name.to_string(), Account { id: None, password, roles, databases: BTreeMap::new() });
            Ok(true)
        })
    }

    /// Sets `name`'s password.
    pub fn set_password(&self, name: &str, password: &str) -> Result<()> {
        check_password(name, password, self.min_password_length)?;
        if !self.read().data.accounts.contains_key(name) {
            return Err(no_user(name));
        }
        let password = PasswordHash::new(password, self.params);
        self.change(|Data { accounts, .. }| {
            accounts.get_mut(name).ok_or_else(|| no_user(name))?.password = password;
            Ok(())
        })
    }

    /// Gives `role` to `name`; returns whether the user lacked it.
    pub fn grant(&self, name: &str, role: Role) -> Result<bool> {
        self.change(|d| Ok(d.accounts.get_mut(name).ok_or_else(|| no_user(name))?.roles.insert(role)))
    }

    /// Takes `role` from `name`; returns whether the user had it.
    pub fn revoke(&self, name: &str, role: Role) -> Result<bool> {
        self.change(|d| Ok(d.accounts.get_mut(name).ok_or_else(|| no_user(name))?.roles.remove(&role)))
    }

    /// Gives `role` (reader or editor) to `name` on database `db` only; returns whether
    /// the user lacked it there.
    pub fn grant_on(&self, name: &str, db: &str, role: Role) -> Result<bool> {
        if role == Role::Admin {
            return Err(Error::Semantic(
                "the admin role is held on every database; grant it without ON DATABASE".into(),
            ));
        }
        self.change(|Data { accounts, .. }| {
            let a = accounts.get_mut(name).ok_or_else(|| no_user(name))?;
            Ok(a.databases.entry(db_key(db)).or_default().insert(role))
        })
    }

    /// Takes `role` from `name` on database `db`; returns whether the user had it there.
    pub fn revoke_on(&self, name: &str, db: &str, role: Role) -> Result<bool> {
        self.change(|Data { accounts, .. }| {
            let a = accounts.get_mut(name).ok_or_else(|| no_user(name))?;
            let key = db_key(db);
            let Some(roles) = a.databases.get_mut(&key) else { return Ok(false) };
            let had = roles.remove(&role);
            if roles.is_empty() {
                a.databases.remove(&key);
            }
            Ok(had)
        })
    }

    /// Removes a user, with their API tokens; returns whether there was one.
    pub fn drop_user(&self, name: &str) -> Result<bool> {
        if !self.read().data.accounts.contains_key(name) {
            return Ok(false);
        }
        self.change(|d| Ok(d.accounts.remove(name).is_some()))
    }

    /// Renames a user, who keeps their API tokens; returns whether there was one to rename.
    pub fn rename(&self, from: &str, to: &str) -> Result<bool> {
        check_name(to)?;
        self.change(|Data { accounts, tokens }| {
            if !accounts.contains_key(from) {
                return Ok(false);
            }
            if accounts.contains_key(to) {
                return Err(Error::Runtime(format!("user `{to}` already exists")));
            }
            let account = accounts.remove(from).expect("checked");
            accounts.insert(to.to_string(), account);
            for t in tokens.values_mut().filter(|t| t.info.user == from) {
                t.info.user = to.to_string();
            }
            Ok(true)
        })
    }
}

/// The API tokens in the system database, by id.
fn load_tokens(db: &Database<BoxedStore>) -> Result<BTreeMap<String, ApiToken>> {
    let bad = |what: &str, id: &str| Error::Storage(format!("system database: invalid {what} of API token `{id}`"));
    let rows = db
        .execute(
            "MATCH (t:ApiToken) RETURN t.id, t.hash, t.name, t.user, t.accountId, t.access, \
             t.databases, t.created, t.expires, t.lastUsed",
        )?
        .rows;
    let mut tokens = BTreeMap::new();
    for row in rows {
        let [id, hash, name, user, account, access, databases, created, expires, last_used] = row.as_slice() else {
            return Err(bad("record", "?"));
        };
        let id = id.as_str().filter(|id| crypto::unhex::<8>(id).is_some()).ok_or_else(|| bad("id", "?"))?;
        let time = |v: &ResultValue, what: &str| match v {
            ResultValue::Null => Ok(None),
            ResultValue::Int(ms) => Ok(Some(*ms)),
            _ => Err(bad(what, id)),
        };
        let databases = match databases {
            ResultValue::List(list) => list.iter().map(|d| d.as_str().map(db_key)).collect::<Option<BTreeSet<String>>>(),
            _ => None,
        };
        let token = ApiToken {
            info: TokenInfo {
                id: id.to_string(),
                name: name.as_str().ok_or_else(|| bad("name", id))?.to_string(),
                user: user.as_str().ok_or_else(|| bad("user", id))?.to_string(),
                access: access.as_str().and_then(|a| TokenAccess::parse(a).ok()).ok_or_else(|| bad("access", id))?,
                databases: databases.ok_or_else(|| bad("databases", id))?,
                created: created.as_int().ok_or_else(|| bad("creation time", id))?,
                expires: time(expires, "expiry")?,
                last_used: time(last_used, "last use")?,
            },
            hash: hash.as_str().and_then(crypto::unhex::<32>).ok_or_else(|| bad("hash", id))?,
            account: account.as_str().ok_or_else(|| bad("account", id))?.to_string(),
        };
        tokens.insert(id.to_string(), token);
    }
    Ok(tokens)
}

/// An API token as the properties of its node.
fn token_record(t: &ApiToken) -> Value {
    let s = |v: &str| Value::String(v.to_string());
    let time = |v: Option<i64>| v.map_or(Value::Null, Value::Int);
    let fields = [
        ("id", s(&t.info.id)),
        ("hash", s(&crypto::hex(&t.hash))),
        ("name", s(&t.info.name)),
        ("user", s(&t.info.user)),
        ("accountId", s(&t.account)),
        ("access", s(t.info.access.name())),
        ("databases", Value::List(t.info.databases.iter().map(|d| s(d)).collect())),
        ("created", Value::Int(t.info.created)),
        ("expires", time(t.info.expires)),
        ("lastUsed", time(t.info.last_used)),
    ];
    Value::Map(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Writes the difference between `old` and `new` to the system database, in one
/// statement: accounts that are gone or renamed and tokens that are gone are deleted, new
/// and changed ones are written.
fn save_system(db: &Database<BoxedStore>, old: &Data, new: &Data) -> Result<()> {
    let drop_tokens: Vec<Value> =
        old.tokens.keys().filter(|id| !new.tokens.contains_key(*id)).map(|id| Value::String(id.clone())).collect();
    let tokens: Vec<Value> =
        new.tokens.iter().filter(|(id, t)| old.tokens.get(*id) != Some(*t)).map(|(_, t)| token_record(t)).collect();
    let (old, new) = (&old.accounts, &new.accounts);
    let remove: Vec<Value> = old
        .keys()
        .filter(|name| !new.contains_key(*name))
        .map(|name| Value::String(name.clone()))
        .collect();
    let upsert: Vec<Value> = new
        .iter()
        .filter(|(name, a)| {
            old.get(*name)
                .is_none_or(|o| o.id != a.id || o.roles != a.roles || o.databases != a.databases || o.password != a.password)
        })
        .map(|(name, a)| {
            Value::Map(BTreeMap::from([
                ("name".to_string(), Value::String(name.clone())),
                ("accountId".to_string(), a.id.clone().map_or(Value::Null, Value::String)),
                ("password".to_string(), Value::String(a.password.encode())),
                (
                    "roles".to_string(),
                    Value::List(a.roles.iter().map(|r| Value::String(r.name().into())).collect()),
                ),
                (
                    "databaseRoles".to_string(),
                    Value::List(
                        a.databases
                            .iter()
                            .flat_map(|(db, roles)| roles.iter().map(move |r| Value::String(format!("{}:{db}", r.name()))))
                            .collect(),
                    ),
                ),
            ]))
        })
        .collect();
    if remove.is_empty() && upsert.is_empty() && drop_tokens.is_empty() && tokens.is_empty() {
        return Ok(());
    }
    db.execute_with_params(
        "OPTIONAL MATCH (old:User) WHERE old.name IN $remove \
         DELETE old \
         WITH count(*) AS ignored \
         OPTIONAL MATCH (old:ApiToken) WHERE old.id IN $dropTokens \
         DELETE old \
         WITH count(*) AS ignored \
         FOREACH (a IN $upsert | \
           MERGE (u:User {name: a.name}) \
           SET u.password = a.password, u.roles = a.roles, u.databaseRoles = a.databaseRoles, u.accountId = a.accountId) \
         FOREACH (t IN $tokens | MERGE (k:ApiToken {id: t.id}) SET k = t)",
        &BTreeMap::from([
            ("remove".to_string(), Value::List(remove)),
            ("upsert".to_string(), Value::List(upsert)),
            ("dropTokens".to_string(), Value::List(drop_tokens)),
            ("tokens".to_string(), Value::List(tokens)),
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
    /// An API token of `user`.
    ApiToken { user: String, id: String },
}

impl Principal {
    /// The name in logs: a user's, with the id of the API token they used; `token` for the
    /// server token, and nothing without authentication.
    pub(crate) fn label(&self) -> String {
        match self {
            Principal::Open => String::new(),
            Principal::Token => "token".into(),
            Principal::User(name) => name.clone(),
            Principal::ApiToken { user, id } => format!("{user}[{TOKEN_PREFIX}{id}]"),
        }
    }
}

/// The name that failed logins with a bearer credential are counted under: an API token's
/// public part, or nothing.
pub(crate) fn bearer_name(credential: &str) -> String {
    token_id(credential).map_or_else(String::new, |id| format!("{TOKEN_PREFIX}{id}"))
}

pub(crate) fn resolve(name: &DbName, params: &BTreeMap<String, Value>, what: &str) -> Result<String> {
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

fn user_row(users: &Users, name: &str, roles: &[Role]) -> Vec<ResultValue> {
    let databases = users.database_roles(name).unwrap_or_default();
    let roles_of = |roles: &[Role]| ResultValue::List(roles.iter().map(|r| ResultValue::String(r.name().into())).collect());
    vec![
        ResultValue::String(name.to_string()),
        roles_of(roles),
        ResultValue::Bool(false),
        ResultValue::Bool(false),
        ResultValue::Null,
        ResultValue::Map(databases.iter().map(|(db, r)| (db.clone(), roles_of(r))).collect()),
    ]
}

fn user_table(rows: Vec<Vec<ResultValue>>) -> QueryResult {
    QueryResult {
        columns: ["user", "roles", "passwordChangeRequired", "suspended", "home", "databaseRoles"]
            .map(String::from)
            .to_vec(),
        rows,
        stats: QueryStats::default(),
        plan: None,
    }
}

fn token_row(t: &TokenInfo) -> Vec<ResultValue> {
    let time = |ms: Option<i64>| {
        ms.and_then(|ms| Temporal::from_epoch_nanos(i128::from(ms) * 1_000_000).ok())
            .map_or(ResultValue::Null, ResultValue::Temporal)
    };
    vec![
        ResultValue::String(t.name.clone()),
        ResultValue::String(t.user.clone()),
        ResultValue::String(t.access.name().into()),
        ResultValue::List(t.databases.iter().map(|d| ResultValue::String(d.clone())).collect()),
        time(Some(t.created)),
        time(t.expires),
        time(t.last_used),
        ResultValue::String(t.id.clone()),
    ]
}

const TOKEN_COLUMNS: [&str; 8] = ["name", "user", "access", "databases", "created", "expires", "lastUsed", "id"];

/// The user whose tokens a command is about: the one it names, which needs the admin role
/// unless it is the client's own, else the client's own.
fn token_owner(
    who: &Principal,
    access: Access,
    user: &Option<DbName>,
    params: &BTreeMap<String, Value>,
    what: &str,
) -> Result<String> {
    let own = match who {
        Principal::User(name) => Some(name),
        _ => None,
    };
    match (user, own) {
        (Some(u), _) => {
            let user = resolve(u, params, "user name")?;
            if own != Some(&user) && access < Access::Admin {
                return Err(Error::Forbidden(format!("{what} FOR USER another user needs the admin role")));
            }
            Ok(user)
        }
        (None, Some(name)) => Ok(name.clone()),
        (None, None) => Err(Error::Semantic(format!("{what} needs FOR USER: this client is not a user account"))),
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
    // An API token never manages accounts, its own user's tokens and password included.
    if matches!(who, Principal::ApiToken { .. }) && !matches!(cmd, UserCommand::ShowCurrentUser) {
        return Err(Error::Forbidden(format!("{} cannot be run with an API token; log in with the password", cmd.name())));
    }
    // These check for themselves.
    let own = matches!(
        cmd,
        UserCommand::ShowCurrentUser
            | UserCommand::AlterCurrent { .. }
            | UserCommand::CreateToken { .. }
            | UserCommand::ShowTokens
            | UserCommand::DropToken { .. }
    );
    if !own && access < Access::Admin {
        return Err(Error::Forbidden(format!("{} needs the admin role", cmd.name())));
    }
    match cmd {
        UserCommand::ShowUsers => Ok(user_table(users.list().iter().map(|(n, r)| user_row(users, n, r)).collect())),
        UserCommand::ShowCurrentUser => {
            let rows = match who {
                Principal::User(name) | Principal::ApiToken { user: name, .. } => users.roles(name).map(|r| user_row(users, name, &r)).into_iter().collect(),
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
        UserCommand::Grant { roles, database, users: names } | UserCommand::Revoke { roles, database, users: names } => {
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
            let database = database.as_ref().map(|d| resolve(d, params, "database name")).transpose()?;
            // Checked before anything changes (grant_on refuses it too).
            if grant && database.is_some() && roles.contains(&Role::Admin) {
                return Err(Error::Semantic(
                    "the admin role is held on every database; grant it without ON DATABASE".into(),
                ));
            }
            let mut n = 0;
            for name in &names {
                for &role in &roles {
                    let did = match (&database, grant) {
                        (None, true) => users.grant(name, role)?,
                        (None, false) => users.revoke(name, role)?,
                        (Some(db), true) => users.grant_on(name, db, role)?,
                        (Some(db), false) => users.revoke_on(name, db, role)?,
                    };
                    n += did as usize;
                }
            }
            Ok(changed(n))
        }
        UserCommand::CreateToken { name, user, write, databases, expires } => {
            let user = token_owner(who, access, user, params, cmd.name())?;
            let name = resolve(name, params, "token name")?;
            let databases =
                databases.iter().map(|d| resolve(d, params, "database name")).collect::<Result<Vec<_>>>()?;
            let token_access = if *write { TokenAccess::Write } else { TokenAccess::Read };
            let lifetime = match expires {
                None => Some(DEFAULT_TOKEN_LIFETIME),
                Some(TokenExpiry::Never) => None,
                Some(TokenExpiry::Hours(h)) => Some(Duration::from_secs(u64::from(*h) * 3600)),
            };
            let (token, info) = users.create_token(&user, &name, token_access, &databases, lifetime)?;
            let mut row = token_row(&info);
            row.push(ResultValue::String(token));
            Ok(QueryResult {
                columns: TOKEN_COLUMNS.into_iter().chain(["token"]).map(String::from).collect(),
                rows: vec![row],
                ..changed(1)
            })
        }
        UserCommand::ShowTokens => {
            let list = match who {
                Principal::User(name) if access < Access::Admin => users.tokens(Some(name)),
                _ => users.tokens(None),
            };
            Ok(QueryResult {
                columns: TOKEN_COLUMNS.map(String::from).to_vec(),
                rows: list.iter().map(token_row).collect(),
                ..changed(0)
            })
        }
        UserCommand::DropToken { name, user, if_exists } => {
            let user = token_owner(who, access, user, params, cmd.name())?;
            let name = resolve(name, params, "token name")?;
            match users.drop_token(&user, &name)? {
                true => Ok(changed(1)),
                false if *if_exists => Ok(changed(0)),
                false => Err(Error::Runtime(format!("user `{user}` has no token `{name}`"))),
            }
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
            // Roles on one database, kept with the account through a rename.
            assert!(users.grant_on("carol", "Movies", Role::Editor).unwrap());
            assert!(users.grant_on("carol", "other", Role::Reader).unwrap());
            assert!(!users.grant_on("carol", "movies", Role::Editor).unwrap());
            assert!(users.grant_on("carol", "movies", Role::Admin).is_err());
            assert!(users.grant_on("robert", "gone", Role::Reader).unwrap());
            assert!(users.revoke_on("robert", "gone", Role::Reader).unwrap());
            assert!(!users.revoke_on("robert", "gone", Role::Reader).unwrap());
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
        assert_eq!(
            users.database_roles("carol").unwrap(),
            BTreeMap::from([("movies".to_string(), vec![Role::Editor]), ("other".to_string(), vec![Role::Reader])])
        );
        assert!(users.database_roles("robert").unwrap().is_empty());
        assert_eq!(users.access("carol"), Some(Access::None));
        assert_eq!(users.access_on("carol", "MOVIES"), Some(Access::Write));
        assert_eq!(users.access_on("carol", "other"), Some(Access::Read));
        assert_eq!(users.access_on("carol", "third"), Some(Access::None));
        assert_eq!(users.best_access("carol"), Some(Access::Write));
        assert_eq!(users.access_on("robert", "movies"), Some(Access::Read));
        // Opening again does not add a second constraint or SystemInfo.
        drop(users);
        Users::system(Arc::clone(&system)).unwrap();
        let info = system.execute("MATCH (i:SystemInfo) RETURN i.version").unwrap();
        assert_eq!(info.rows, vec![vec![ResultValue::Int(SYSTEM_VERSION)]]);
        // The constraint holds even for writes that bypass Users.
        assert!(system.execute("CREATE (:User {name: 'alice'})").is_err());
    }

    #[test]
    fn tokens_are_capped_by_their_user_access_and_databases() {
        let users = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
        users.create("ann", "pw", &[Role::Admin], ExistingDatabase::Fail).unwrap();
        users.create("bob", "pw", &[], ExistingDatabase::Fail).unwrap();
        users.grant_on("bob", "movies", Role::Editor).unwrap();
        let make = |user: &str, name: &str, access, dbs: &[&str]| {
            let dbs: Vec<String> = dbs.iter().map(|d| d.to_string()).collect();
            let (token, info) = users.create_token(user, name, access, &dbs, None).unwrap();
            assert!(token.starts_with(&format!("gbt_{}_", info.id)) && token.len() == 85, "{token}");
            assert_eq!(token_id(&token), Some(info.id.as_str()));
            assert_eq!(
                users.authenticate_token(&token),
                Some(Principal::ApiToken { user: user.into(), id: info.id.clone() })
            );
            info.id
        };
        let all = |id: &str| {
            (
                users.token_access(id).unwrap(),
                users.token_access_on(id, "Movies").unwrap(),
                users.token_access_on(id, "other").unwrap(),
                users.token_best_access(id).unwrap(),
            )
        };
        use Access::{None as No, Read, Write};
        // Never admin, also an administrator's.
        assert_eq!(all(&make("ann", "r", TokenAccess::Read, &[])), (Read, Read, Read, Read));
        let w = make("ann", "w", TokenAccess::Write, &[]);
        assert_eq!(all(&w), (Write, Write, Write, Write));
        assert_eq!(all(&make("ann", "m", TokenAccess::Write, &["MOVIES"])), (No, Write, No, Write));
        // Never more than the user.
        assert_eq!(all(&make("bob", "w", TokenAccess::Write, &[])), (No, Write, No, Write));
        assert_eq!(all(&make("bob", "r", TokenAccess::Read, &[])), (No, Read, No, Read));
        assert_eq!(all(&make("bob", "o", TokenAccess::Write, &["other"])), (No, No, No, No));
        // A role taken away applies at once.
        users.grant("bob", Role::Admin).unwrap();
        users.revoke("ann", Role::Admin).unwrap();
        assert_eq!(all(&w), (No, No, No, No));

        // Names are per user.
        assert!(users.create_token("ann", "w", TokenAccess::Read, &[], None).is_err());
        assert!(users.create_token("ann", "a b", TokenAccess::Read, &[], None).is_err());
        assert!(users.create_token("nobody", "x", TokenAccess::Read, &[], None).is_err());
        assert_eq!(users.tokens(Some("ann")).len(), 3);
        assert_eq!(users.tokens(None).len(), 6);
        for i in users.tokens(Some("ann")).len()..MAX_TOKENS {
            users.create_token("ann", &format!("t{i}"), TokenAccess::Read, &[], None).unwrap();
        }
        assert!(users.create_token("ann", "more", TokenAccess::Read, &[], None).is_err());
    }

    #[test]
    fn tokens_expire_and_end_with_their_account() {
        let users = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
        users.create("ann", "pw", &[Role::Editor], ExistingDatabase::Fail).unwrap();
        let make = |user: &str, name: &str, life| users.create_token(user, name, TokenAccess::Write, &[], life).unwrap();

        // Expired: it is who it is, but may do nothing.
        let (token, info) = make("ann", "old", Some(Duration::ZERO));
        assert!(users.authenticate_token(&token).is_some());
        assert_eq!(users.token_access(&info.id), None);
        assert_eq!(users.token_access_on(&info.id, "graphbuff"), None);
        assert_eq!(users.token_best_access(&info.id), None);
        assert_eq!(users.tokens(Some("ann"))[0].last_used, None);

        let (token, info) = make("ann", "new", Some(Duration::from_secs(3600)));
        assert!(users.token_access(&info.id).is_some());
        // Wrong secrets, and what is no token at all.
        let other = make("ann", "other", None).0;
        let mixed = format!("{}{}", &token[..21], &other[21..]);
        for bad in [mixed.as_str(), &token[..84], "gbt_", "", "pw", &token.to_uppercase(), &format!("{token}0")] {
            assert!(users.authenticate_token(bad).is_none(), "{bad}");
        }
        // A token is not a password, and a password not a token.
        assert!(!users.authenticate("ann", &token));
        assert!(users.authenticate_token(&token).is_some());
        assert!(users.tokens(Some("ann")).iter().any(|t| t.name == "new" && t.last_used.is_some()));

        // It stays with its user through a rename and a new password.
        users.rename("ann", "anna").unwrap();
        users.set_password("anna", "pw2").unwrap();
        let who = Principal::ApiToken { user: "anna".into(), id: info.id.clone() };
        assert_eq!(users.authenticate_token(&token), Some(who));
        assert_eq!(users.token_access(&info.id), Some(Access::Write));
        // Dropping it.
        assert!(users.drop_token("anna", "other").unwrap());
        assert!(!users.drop_token("anna", "other").unwrap());
        assert!(users.drop_token("nobody", "other").is_err());
        assert!(users.authenticate_token(&other).is_none());
        // Replacing or dropping the user drops their tokens, also once the name is back.
        users.create("anna", "pw", &[Role::Editor], ExistingDatabase::Replace).unwrap();
        assert!(users.authenticate_token(&token).is_none());
        assert_eq!(users.token_access(&info.id), None);
        let (token, info) = make("anna", "new", None);
        users.drop_user("anna").unwrap();
        users.create("anna", "pw", &[Role::Editor], ExistingDatabase::Fail).unwrap();
        assert!(users.authenticate_token(&token).is_none());
        assert_eq!(users.token_best_access(&info.id), None);
        assert!(users.tokens(None).is_empty());
    }

    #[test]
    fn tokens_persist_in_the_system_database() {
        let tmp = TempDir::new("tokens");
        let dir = &tmp.0;
        let open = |dir: &std::path::Path| {
            let mgr = graphbuff::DatabaseManager::open(dir).unwrap();
            let system = mgr.system_database().unwrap();
            let users = Users::system(Arc::clone(&system)).unwrap().with_hash_params(HashParams::TESTING).with_min_password_length(1);
            (mgr, system, users)
        };
        let (kept, limited, gone, other);
        {
            let (_mgr, system, users) = open(dir);
            users.create("ann", "pw", &[Role::Editor], ExistingDatabase::Fail).unwrap();
            users.create("bob", "pw", &[Role::Reader], ExistingDatabase::Fail).unwrap();
            let make = |user: &str, name: &str, access, dbs: &[String], life| {
                users.create_token(user, name, access, dbs, life).unwrap().0
            };
            kept = make("ann", "kept", TokenAccess::Write, &[], None);
            limited = make("ann", "limited", TokenAccess::Read, &["b".into(), "a".into()], Some(DEFAULT_TOKEN_LIFETIME));
            gone = make("ann", "gone", TokenAccess::Read, &[], None);
            other = make("bob", "kept", TokenAccess::Read, &[], None);
            assert!(users.drop_token("ann", "gone").unwrap());
            assert!(users.authenticate_token(&kept).is_some());
            users.rename("ann", "anna").unwrap();
            // Only hashes are stored.
            let all = format!("{:?}", system.execute("MATCH (n) RETURN n").unwrap().rows);
            assert!(all.contains(token_id(&kept).unwrap()));
            for t in [&kept, &limited, &gone, &other] {
                assert!(!all.contains(&t[21..]), "the secret is stored");
            }
        }
        {
            let (_mgr, system, users) = open(dir);
            let list = users.tokens(None);
            let names: Vec<_> = list.iter().map(|t| (t.user.as_str(), t.name.as_str(), t.access)).collect();
            assert_eq!(
                names,
                [
                    ("anna", "kept", TokenAccess::Write),
                    ("anna", "limited", TokenAccess::Read),
                    ("bob", "kept", TokenAccess::Read)
                ]
            );
            assert!(list[0].last_used.is_some() && list[0].expires.is_none() && list[0].databases.is_empty());
            assert!(list[1].last_used.is_none());
            assert_eq!(list[1].expires, Some(list[1].created + DEFAULT_TOKEN_LIFETIME.as_millis() as i64));
            assert_eq!(list[1].databases, BTreeSet::from(["a".to_string(), "b".to_string()]));
            for t in [&kept, &limited, &other] {
                assert!(users.authenticate_token(t).is_some());
            }
            assert!(users.authenticate_token(&gone).is_none());
            // Account changes leave tokens alone.
            users.grant("bob", Role::Editor).unwrap();
            assert!(users.authenticate_token(&other).is_some());

            // A version without tokens drops and re-creates a user: it ignores the tokens,
            // and they do not come back to life.
            drop(users);
            system.execute("MATCH (u:User {name: 'bob'}) DELETE u").unwrap();
            let hash = PasswordHash::new("pw", HashParams::TESTING).encode();
            system
                .execute_with_params(
                    "CREATE (:User {name: 'bob', password: $p, roles: ['admin'], databaseRoles: []})",
                    &BTreeMap::from([("p".to_string(), Value::String(hash))]),
                )
                .unwrap();
        }
        let (_mgr, system, users) = open(dir);
        assert!(users.authenticate_token(&other).is_none());
        assert!(users.authenticate_token(&kept).is_some());
        users.create_token("bob", "fresh", TokenAccess::Read, &[], None).unwrap();
        assert!(users.authenticate_token(&other).is_none());
        // Dropping a user removes their tokens from the database.
        users.drop_user("anna").unwrap();
        let left = system.execute("MATCH (t:ApiToken) WHERE t.user = 'anna' RETURN count(t)").unwrap();
        assert_eq!(left.rows, vec![vec![ResultValue::Int(0)]]);
        // A damaged record is refused, not skipped.
        drop(users);
        system.execute("MATCH (t:ApiToken {name: 'fresh'}) SET t.access = 'admin'").unwrap();
        assert!(Users::system(system).is_err());
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

        // API tokens: one's own, or anyone's for administrators.
        let made = run_as(&kim, Access::Write, "CREATE TOKEN laptop ACCESS WRITE EXPIRES IN 2 DAYS").unwrap();
        assert_eq!(made.columns.last().unwrap(), "token");
        let info = &users.tokens(Some("kim"))[0];
        assert_eq!((info.access, info.expires), (TokenAccess::Write, Some(info.created + 2 * 24 * 3_600_000)));
        let secret = made.rows[0].last().unwrap().as_str().unwrap().to_string();
        let token = users.authenticate_token(&secret).unwrap();
        run_as(&kim, Access::Write, "CREATE TOKEN mine FOR USER kim EXPIRES NEVER").unwrap();
        assert_eq!(users.tokens(Some("kim"))[1].expires, None);
        for refused in ["CREATE TOKEN x FOR USER root", "DROP TOKEN x FOR USER root", "CREATE TOKEN x FOR USER ghost"] {
            assert!(matches!(run_as(&kim, Access::Write, refused), Err(Error::Forbidden(_))), "{refused}");
        }
        run_as(&admin, Access::Admin, "CREATE TOKEN given FOR USER kim").unwrap();
        run_as(&admin, Access::Admin, "CREATE TOKEN own").unwrap();
        assert_eq!(run_as(&kim, Access::Write, "SHOW TOKENS").unwrap().rows.len(), 3);
        let shown = run_as(&admin, Access::Admin, "SHOW TOKENS").unwrap();
        assert_eq!(shown.rows.len(), 4);
        assert!(!format!("{shown:?}").contains(&secret[21..]));
        run_as(&kim, Access::Write, "DROP TOKEN given").unwrap();
        assert!(run_as(&kim, Access::Write, "DROP TOKEN given").is_err());
        assert_eq!(run_as(&kim, Access::Write, "DROP TOKEN given IF EXISTS").unwrap().stats.system_updates, 0);
        // The server token has no user of its own.
        assert!(run_as(&Principal::Token, Access::Admin, "CREATE TOKEN x").is_err());
        run_as(&Principal::Token, Access::Admin, "CREATE TOKEN x FOR USER kim").unwrap();
        // A token manages nothing, whatever access is claimed for it.
        for refused in [
            "CREATE TOKEN y",
            "CREATE TOKEN y FOR USER kim",
            "SHOW TOKENS",
            "DROP TOKEN mine",
            "ALTER CURRENT USER SET PASSWORD FROM 'n' TO 'm'",
            "SHOW USERS",
            "CREATE USER y SET PASSWORD 'y'",
            "GRANT ROLE admin TO kim",
        ] {
            assert!(matches!(run_as(&token, Access::Admin, refused), Err(Error::Forbidden(_))), "{refused}");
        }
        let me = run_as(&token, Access::Write, "SHOW CURRENT USER").unwrap();
        assert_eq!(me.rows[0][0], ResultValue::String("kim".into()));

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
