//! OAuth 2.1 for hosted MCP clients (claude.ai, ChatGPT): `graphbuff-server` is its own
//! authorization server, on the MCP port, when `mcp.public_url` is set.
//!
//! - Discovery: protected resource metadata (RFC 9728) and authorization server metadata
//!   (RFC 8414, and the OpenID path), `WWW-Authenticate` with `resource_metadata` on 401.
//! - `/oauth/authorize`: a login and consent page; the code flow with PKCE (S256 only),
//!   `iss` in the answer (RFC 9207), `resource` checked when sent (RFC 8707).
//! - `/oauth/token`: codes (once, within a minute) and rotating refresh tokens (one used
//!   twice ends its grant). Access tokens are API tokens that work for an hour, only here.
//! - `/oauth/revoke` (RFC 7009).
//! - Clients: client ID metadata documents (an `https://` URL as `client_id`, fetched
//!   with care: public addresses only, no redirects, 64 KiB, 5 s), and clients listed in
//!   `mcp.oauth_clients`. All are public clients (`none`): PKCE stands in for a secret.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::api::{Ctx, LoginError};
use crate::crypto;
use crate::http::{Request, Response, form_fields, percent_encode};
use crate::json::{self, Json};
use crate::throttle;
use crate::users::{Access, Principal, TokenAccess};

use super::rpc::{encode, obj, string};

/// How long a login and consent page may be left open.
const PENDING_LIFETIME: Duration = Duration::from_secs(600);
/// How long a code works.
const CODE_LIFETIME: Duration = Duration::from_secs(60);
/// Pages and codes kept at once, against a flood of them.
const MAX_PENDING: usize = 1000;
/// How long a fetched client metadata document is kept.
const DOCUMENT_LIFETIME: Duration = Duration::from_secs(600);
const MAX_DOCUMENTS: usize = 1000;

/// A client listed in `mcp.oauth_clients`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthClient {
    pub id: String,
    /// Where it may be sent back to: exact URLs, or loopback ones (any port).
    pub redirect_uris: Vec<String>,
}

impl OAuthClient {
    /// Parses `mcp.oauth_clients`: `client_id redirect_uri [redirect_uri…]`, clients
    /// separated by `;`.
    pub fn parse_list(text: &str) -> Result<Vec<OAuthClient>, String> {
        let mut out = Vec::new();
        for part in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let mut words = part.split_whitespace();
            let id = words.next().unwrap_or_default().to_string();
            let redirect_uris: Vec<String> = words.map(String::from).collect();
            if redirect_uris.is_empty() {
                return Err(format!("client `{id}` needs at least one redirect URI after its id"));
            }
            for r in &redirect_uris {
                check_redirect(r).map_err(|e| format!("client `{id}`: {e}"))?;
            }
            if out.iter().any(|c: &OAuthClient| c.id == id) {
                return Err(format!("client `{id}` is listed twice"));
            }
            out.push(OAuthClient { id, redirect_uris });
        }
        Ok(out)
    }
}

/// Checks the public URL of the MCP endpoint: `https://host[:port]/mcp`. Returns the
/// issuer, `https://host[:port]`.
pub fn check_public_url(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("https://").ok_or("mcp.public_url must start with https://")?;
    let (authority, path) = rest.split_once('/').ok_or("mcp.public_url needs the path /mcp")?;
    if path != "mcp" {
        return Err("mcp.public_url must end in /mcp (a proxy in front may forward to the MCP port)".into());
    }
    if authority.is_empty() || authority.contains(['@', '?', '#', ' ']) {
        return Err("mcp.public_url has no valid host".into());
    }
    Ok(format!("https://{authority}"))
}

/// Whether `uri` may be a redirect URI: `https://`, or `http://` to a loopback host.
fn check_redirect(uri: &str) -> Result<(), String> {
    if uri.contains('#') {
        return Err(format!("redirect URI `{uri}` has a fragment"));
    }
    if uri.starts_with("https://") && uri.len() > 8 {
        return Ok(());
    }
    if loopback(uri).is_some() {
        return Ok(());
    }
    Err(format!("redirect URI `{uri}` must be https://, or http:// to localhost, 127.0.0.1 or [::1]"))
}

/// A loopback `http://` URI without its port: `(host, path and query)`.
fn loopback(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = if let Some(v6) = authority.strip_prefix('[') {
        let end = v6.find(']')?;
        let after = &v6[end + 1..];
        if !after.is_empty() && !after.strip_prefix(':')?.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        &authority[..end + 2]
    } else {
        match authority.split_once(':') {
            Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
            Some(_) => return None,
            None => authority,
        }
    };
    ["localhost", "127.0.0.1", "[::1]"].contains(&host.to_ascii_lowercase().as_str()).then_some((host, path))
}

/// Whether `requested` is one of `registered`: the same text, or for loopback URIs the
/// same but for the port (RFC 8252).
fn redirect_matches(registered: &[String], requested: &str) -> bool {
    registered.iter().any(|r| {
        r == requested
            || matches!((loopback(r), loopback(requested)), (Some((h1, p1)), Some((h2, p2))) if h1.eq_ignore_ascii_case(h2) && p1 == p2)
    })
}

/// A client as the server knows it.
#[derive(Clone, Debug)]
struct Client {
    id: String,
    /// What the consent page calls it.
    name: String,
    redirect_uris: Vec<String>,
}

/// A login and consent page that was shown.
struct Pending {
    client: Client,
    redirect_uri: String,
    state: Option<String>,
    challenge: String,
    /// Whether the client asked to write.
    wants_write: bool,
    /// Sent back in the form (against forgery), and set as a cookie.
    csrf: String,
    created: Instant,
}

/// A code given to a client, and what it stands for.
struct Code {
    client: String,
    redirect_uri: String,
    challenge: String,
    user: String,
    access: TokenAccess,
    databases: Vec<String>,
    created: Instant,
    /// The refresh token issued for it: a code used twice ends that grant.
    used: Option<String>,
}

/// The authorization server's state.
pub(crate) struct OAuth {
    /// `https://host/mcp`: the resource, and the audience of the tokens.
    pub public_url: String,
    /// `https://host`.
    pub issuer: String,
    clients: Vec<OAuthClient>,
    /// Hosts whose client metadata documents are fetched; empty: any public one.
    client_hosts: Vec<String>,
    pending: Mutex<HashMap<String, Pending>>,
    /// By the SHA-256 of the code.
    codes: Mutex<HashMap<[u8; 32], Code>>,
    documents: Mutex<HashMap<String, (Instant, Client)>>,
}

impl OAuth {
    pub(crate) fn new(public_url: &str, clients: Vec<OAuthClient>, client_hosts: Vec<String>) -> Result<OAuth, String> {
        let issuer = check_public_url(public_url)?;
        Ok(OAuth {
            public_url: public_url.to_string(),
            issuer,
            clients,
            client_hosts: client_hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect(),
            pending: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
            documents: Mutex::new(HashMap::new()),
        })
    }

    /// Where the protected resource metadata is, for `WWW-Authenticate`.
    pub(crate) fn resource_metadata(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource/mcp", self.issuer)
    }

    /// The `WWW-Authenticate` header of a 401 at the MCP endpoint.
    pub(crate) fn challenge(&self) -> String {
        format!("Bearer resource_metadata=\"{}\", scope=\"read\"", self.resource_metadata())
    }

    /// The response to `req` if it is one of the OAuth paths.
    pub(crate) fn handle(&self, ctx: &Ctx, req: &Request, peer: &str) -> Option<Response> {
        let path = req.path.as_str();
        Some(match (req.method.as_str(), path) {
            ("GET", "/.well-known/oauth-protected-resource" | "/.well-known/oauth-protected-resource/mcp") => {
                self.protected_resource()
            }
            ("GET", "/.well-known/oauth-authorization-server" | "/.well-known/openid-configuration") => self.metadata(),
            ("GET", "/oauth/authorize") => self.authorize_page(&form_fields(&req.query)),
            ("POST", "/oauth/authorize") => self.authorize_answer(ctx, req, peer),
            ("POST", "/oauth/token") => self.token(ctx, req),
            ("POST", "/oauth/revoke") => self.revoke(ctx, req),
            (_, "/oauth/authorize") => Response::text(405, "GET or POST".into(), "text/plain").with_header("Allow", "GET, POST"),
            (_, "/oauth/token" | "/oauth/revoke") => Response::text(405, "POST only".into(), "text/plain").with_header("Allow", "POST"),
            (_, p) if p.starts_with("/.well-known/") => Response::text(404, "not found".into(), "text/plain"),
            _ => return None,
        })
    }

    fn protected_resource(&self) -> Response {
        let body = obj([
            ("resource", string(self.public_url.clone())),
            ("authorization_servers", Json::Array(vec![string(self.issuer.clone())])),
            ("scopes_supported", Json::Array(vec![string("read"), string("write")])),
            ("bearer_methods_supported", Json::Array(vec![string("header")])),
            ("resource_name", string("GraphBuff")),
        ]);
        Response::json(200, encode(&body)).with_header("Cache-Control", "max-age=300")
    }

    fn metadata(&self) -> Response {
        let i = &self.issuer;
        let list = |items: &[&str]| Json::Array(items.iter().map(|s| string(*s)).collect());
        let body = obj([
            ("issuer", string(i.clone())),
            ("authorization_endpoint", string(format!("{i}/oauth/authorize"))),
            ("token_endpoint", string(format!("{i}/oauth/token"))),
            ("revocation_endpoint", string(format!("{i}/oauth/revoke"))),
            ("response_types_supported", list(&["code"])),
            ("response_modes_supported", list(&["query"])),
            ("grant_types_supported", list(&["authorization_code", "refresh_token"])),
            ("code_challenge_methods_supported", list(&["S256"])),
            ("token_endpoint_auth_methods_supported", list(&["none"])),
            ("revocation_endpoint_auth_methods_supported", list(&["none"])),
            ("scopes_supported", list(&["read", "write"])),
            ("client_id_metadata_document_supported", Json::Bool(true)),
            ("authorization_response_iss_parameter_supported", Json::Bool(true)),
        ]);
        Response::json(200, encode(&body)).with_header("Cache-Control", "max-age=300")
    }

    // ---- clients ----------------------------------------------------------------------

    /// The client `id`: listed, or described by the metadata document at `id`.
    fn client(&self, id: &str) -> Result<Client, String> {
        if let Some(c) = self.clients.iter().find(|c| c.id == id) {
            return Ok(Client { id: c.id.clone(), name: c.id.clone(), redirect_uris: c.redirect_uris.clone() });
        }
        if !id.starts_with("https://") {
            return Err(format!("unknown client `{id}`: it is not in mcp.oauth_clients, and not an https:// URL of a client metadata document"));
        }
        if let Some((at, c)) = self.documents.lock().unwrap_or_else(|e| e.into_inner()).get(id)
            && at.elapsed() < DOCUMENT_LIFETIME
        {
            return Ok(c.clone());
        }
        let text = crate::fetch::get_public(id, &self.client_hosts).map_err(|e| format!("the client's metadata document {id}: {e}"))?;
        let c = parse_document(id, &text)?;
        let mut docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        if docs.len() >= MAX_DOCUMENTS {
            docs.retain(|_, (at, _)| at.elapsed() < DOCUMENT_LIFETIME);
        }
        if docs.len() < MAX_DOCUMENTS {
            docs.insert(id.to_string(), (Instant::now(), c.clone()));
        }
        Ok(c)
    }

    // ---- authorize --------------------------------------------------------------------

    fn authorize_page(&self, q: &[(String, String)]) -> Response {
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        if let Some((k, _)) = q.iter().enumerate().find(|(i, (k, _))| q[..*i].iter().any(|(n, _)| n == k)).map(|(_, kv)| kv) {
            return error_page(400, &format!("`{k}` is given twice"));
        }
        // Until the client and its redirect URI are known good, errors are shown here,
        // not sent back (that would make this an open redirect).
        let Some(client_id) = get("client_id") else { return error_page(400, "the request has no client_id") };
        let client = match self.client(client_id) {
            Ok(c) => c,
            Err(e) => return error_page(400, &e),
        };
        let redirect_uri = match get("redirect_uri") {
            Some(r) => r.to_string(),
            None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
            None => return error_page(400, "the request has no redirect_uri"),
        };
        if check_redirect(&redirect_uri).is_err() || !redirect_matches(&client.redirect_uris, &redirect_uri) {
            return error_page(400, &format!("{redirect_uri} is not a redirect URI of client {}", client.id));
        }
        let state = get("state").map(String::from);
        let back = |error: &str, why: &str| self.redirect_error(&redirect_uri, state.as_deref(), error, why);
        if get("response_type") != Some("code") {
            return back("unsupported_response_type", "only response_type=code is supported");
        }
        let Some(challenge) = get("code_challenge").filter(|c| (43..=128).contains(&c.len())) else {
            return back("invalid_request", "PKCE is required: send code_challenge (S256)");
        };
        if get("code_challenge_method") != Some("S256") {
            return back("invalid_request", "code_challenge_method must be S256");
        }
        if let Some(r) = get("resource")
            && r.trim_end_matches('/') != self.public_url
        {
            return back("invalid_target", &format!("this server's resource is {}", self.public_url));
        }
        let scopes: Vec<&str> = get("scope").unwrap_or("read").split_whitespace().collect();
        if let Some(s) = scopes.iter().find(|s| !["read", "write"].contains(s)) {
            return back("invalid_scope", &format!("unknown scope `{s}`; the scopes are read and write"));
        }
        let (id, csrf) = match (random(), random()) {
            (Some(a), Some(b)) => (a, b),
            _ => return error_page(500, "no random numbers"),
        };
        let pending = Pending {
            client,
            redirect_uri,
            state,
            challenge: challenge.to_string(),
            wants_write: scopes.contains(&"write"),
            csrf: csrf.clone(),
            created: Instant::now(),
        };
        let page = consent_page(&id, &pending, None);
        {
            let mut all = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            all.retain(|_, p| p.created.elapsed() < PENDING_LIFETIME);
            if all.len() >= MAX_PENDING {
                return error_page(503, "too many logins are under way; try again in a few minutes");
            }
            all.insert(id.clone(), pending);
        }
        html(200, page).with_header(
            "Set-Cookie",
            format!("gbo_{id}={csrf}; Path=/oauth; Max-Age=600; HttpOnly; Secure; SameSite=Strict"),
        )
    }

    fn redirect_error(&self, redirect_uri: &str, state: Option<&str>, error: &str, why: &str) -> Response {
        let mut params = vec![("error", error.to_string()), ("error_description", why.to_string())];
        if let Some(s) = state {
            params.push(("state", s.to_string()));
        }
        params.push(("iss", self.issuer.clone()));
        redirect(redirect_uri, &params)
    }

    fn authorize_answer(&self, ctx: &Ctx, req: &Request, peer: &str) -> Response {
        let f = form_fields(&String::from_utf8_lossy(&req.body));
        let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let Some(id) = get("request") else { return error_page(400, "the form has no request") };
        let cookie = cookie(req, &format!("gbo_{id}"));
        let mut all = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = all.get(id).filter(|p| p.created.elapsed() < PENDING_LIFETIME) else {
            return error_page(400, "this page has expired: go back to the app and connect again");
        };
        if !cookie.is_some_and(|c| api_same(c, &p.csrf)) || !get("csrf").is_some_and(|c| api_same(c, &p.csrf)) {
            return error_page(403, "this form was not sent from the page this server showed (check that cookies are on)");
        }
        if get("action") != Some("allow") {
            let p = all.remove(id).expect("checked");
            return self.redirect_error(&p.redirect_uri, p.state.as_deref(), "access_denied", "the user did not allow access");
        }
        let user = get("user").unwrap_or("").trim().to_string();
        let password = get("password").unwrap_or("");
        let Some(users) = ctx.users.as_ref() else {
            return error_page(400, "this server has no user accounts to log in with");
        };
        let login = ctx.throttled_login("oauth", peer, &user, true, || {
            (!user.is_empty() && users.authenticate(&user, password)).then(|| Principal::User(user.clone()))
        });
        let who = match login {
            Ok(w) => w,
            Err(LoginError::Locked(wait)) => {
                let page = consent_page(id, p, Some(&format!("too many failed logins: try again in {} s", throttle::seconds(wait))));
                return html(429, page);
            }
            Err(LoginError::Wrong) => return html(401, consent_page(id, p, Some("wrong user name or password"))),
        };
        let best = super::cap(ctx.best_access(&who));
        if best < Access::Read {
            return html(403, consent_page(id, p, Some(&format!("user `{user}` may not read any database"))));
        }
        let access = if get("access") == Some("write") && best >= Access::Write { TokenAccess::Write } else { TokenAccess::Read };
        let mut databases = Vec::new();
        for d in get("databases").unwrap_or("").split(',').map(str::trim).filter(|d| !d.is_empty()) {
            if ctx.mgr.database(d).is_err() || ctx.access_on(&who, &d.to_ascii_lowercase()) < Access::Read {
                return html(400, consent_page(id, p, Some(&format!("no database `{d}` that you may read"))));
            }
            databases.push(d.to_ascii_lowercase());
        }
        let p = all.remove(id).expect("checked");
        drop(all);
        let Some(code) = random() else { return error_page(500, "no random numbers") };
        let code = format!("gbc_{code}");
        {
            let mut codes = self.codes.lock().unwrap_or_else(|e| e.into_inner());
            codes.retain(|_, c| c.created.elapsed() < CODE_LIFETIME * 10);
            if codes.len() >= MAX_PENDING {
                return error_page(503, "too many logins are under way; try again in a minute");
            }
            codes.insert(
                crypto::sha256(code.as_bytes()),
                Code {
                    client: p.client.id.clone(),
                    redirect_uri: p.redirect_uri.clone(),
                    challenge: p.challenge,
                    user,
                    access,
                    databases,
                    created: Instant::now(),
                    used: None,
                },
            );
        }
        if let Some(log) = &ctx.log {
            log.security(format_args!(
                "oauth {peer} `{}` CONSENT: client {} gets {} access",
                p.client.id.replace(['\r', '\n'], " "),
                p.client.name.replace(['\r', '\n'], " "),
                access.name()
            ));
        }
        let mut params = vec![("code", code)];
        if let Some(s) = p.state {
            params.push(("state", s));
        }
        params.push(("iss", self.issuer.clone()));
        redirect(&p.redirect_uri, &params)
    }

    // ---- token ------------------------------------------------------------------------

    fn token(&self, ctx: &Ctx, req: &Request) -> Response {
        let f = form_fields(&String::from_utf8_lossy(&req.body));
        let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        if req.header("authorization").is_some() {
            return token_error(401, "invalid_client", "clients here are public: send client_id in the body, no credentials");
        }
        let Some(users) = ctx.users.as_ref() else {
            return token_error(400, "invalid_request", "this server has no user accounts");
        };
        let Some(client) = get("client_id") else { return token_error(400, "invalid_request", "client_id is required") };
        if let Some(r) = get("resource")
            && r.trim_end_matches('/') != self.public_url
        {
            return token_error(400, "invalid_target", &format!("this server's resource is {}", self.public_url));
        }
        let issued = match get("grant_type") {
            Some("authorization_code") => {
                let (Some(code), Some(verifier)) = (get("code"), get("code_verifier")) else {
                    return token_error(400, "invalid_request", "code and code_verifier are required");
                };
                let mut codes = self.codes.lock().unwrap_or_else(|e| e.into_inner());
                let Some(c) = codes.get_mut(&crypto::sha256(code.as_bytes())) else {
                    return token_error(400, "invalid_grant", "unknown or expired code");
                };
                if let Some(refresh) = c.used.take() {
                    users.oauth_revoke(&refresh);
                    return token_error(400, "invalid_grant", "the code was used already; the tokens it gave are revoked");
                }
                if c.created.elapsed() >= CODE_LIFETIME
                    || c.client != client
                    || get("redirect_uri").is_some_and(|r| r != c.redirect_uri)
                    || !pkce_ok(verifier, &c.challenge)
                {
                    c.used = Some(String::new());
                    return token_error(400, "invalid_grant", "the code does not fit this client, redirect URI or verifier, or has expired");
                }
                match users.oauth_grant(&c.user, &c.client, c.access, &c.databases, &self.public_url) {
                    Ok(i) => {
                        c.used = Some(i.refresh_token.clone());
                        i
                    }
                    Err(e) => return token_error(400, "invalid_grant", &e.to_string()),
                }
            }
            Some("refresh_token") => {
                let Some(r) = get("refresh_token") else { return token_error(400, "invalid_request", "refresh_token is required") };
                match users.oauth_refresh(r, client) {
                    Some(i) => i,
                    None => return token_error(400, "invalid_grant", "the refresh token does not work (any more)"),
                }
            }
            _ => return token_error(400, "unsupported_grant_type", "grant_type is authorization_code or refresh_token"),
        };
        let body = obj([
            ("access_token", string(issued.access_token)),
            ("token_type", string("Bearer")),
            ("expires_in", Json::Int(issued.expires_in as i64)),
            ("refresh_token", string(issued.refresh_token)),
            ("scope", string(issued.access.name())),
        ]);
        Response::json(200, encode(&body)).with_header("Cache-Control", "no-store").with_header("Pragma", "no-cache")
    }

    fn revoke(&self, ctx: &Ctx, req: &Request) -> Response {
        let f = form_fields(&String::from_utf8_lossy(&req.body));
        if let Some((_, t)) = f.iter().find(|(n, _)| n == "token")
            && let Some(users) = &ctx.users
        {
            users.oauth_revoke(t);
        }
        Response::json(200, "{}".into()).with_header("Cache-Control", "no-store")
    }
}

/// Checks a client metadata document fetched from `url`.
fn parse_document(url: &str, text: &str) -> Result<Client, String> {
    let Ok(Json::Object(m)) = json::parse(text) else { return Err(format!("{url} is not a JSON object")) };
    match m.get("client_id") {
        Some(Json::String(id)) if id == url => {}
        _ => return Err(format!("the client_id in {url} is not that URL")),
    }
    let name = match m.get("client_name") {
        Some(Json::String(n)) if !n.trim().is_empty() => n.trim().chars().take(100).collect(),
        _ => return Err(format!("{url} has no client_name")),
    };
    let redirect_uris: Vec<String> = match m.get("redirect_uris") {
        Some(Json::Array(list)) if !list.is_empty() => list
            .iter()
            .map(|r| match r {
                Json::String(r) => check_redirect(r).map(|_| r.clone()),
                _ => Err(format!("a redirect URI in {url} is not a string")),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(format!("{url} has no redirect_uris")),
    };
    match m.get("token_endpoint_auth_method") {
        None | Some(Json::Null) => {}
        Some(Json::String(s)) if s == "none" => {}
        Some(_) => return Err(format!("{url}: this server takes public clients only (token_endpoint_auth_method none)")),
    }
    Ok(Client { id: url.to_string(), name, redirect_uris })
}

/// `base64url(sha256(verifier)) == challenge`, and the verifier is well formed.
fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    if !(43..=128).contains(&verifier.len()) || !verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)) {
        return false;
    }
    let b64: String = crypto::base64_encode(&crypto::sha256(verifier.as_bytes()))
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    api_same(&b64, challenge)
}

fn api_same(a: &str, b: &str) -> bool {
    crate::api::same_secret(a, b)
}

fn random() -> Option<String> {
    crypto::secret_bytes::<16>().ok().map(|b| crypto::hex(&b))
}

fn cookie<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.header("cookie")?
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
}

fn redirect(uri: &str, params: &[(&str, String)]) -> Response {
    let mut to = uri.to_string();
    let mut sep = if uri.contains('?') { '&' } else { '?' };
    for (k, v) in params {
        to.push(sep);
        to.push_str(k);
        to.push('=');
        to.push_str(&percent_encode(v));
        sep = '&';
    }
    Response::text(302, String::new(), "text/plain").with_header("Location", to).with_header("Cache-Control", "no-store")
}

fn token_error(status: u16, error: &str, why: &str) -> Response {
    let body = obj([("error", string(error)), ("error_description", string(why))]);
    let r = Response::json(status, encode(&body)).with_header("Cache-Control", "no-store");
    if status == 401 { r.with_header("WWW-Authenticate", "Basic") } else { r }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn html(status: u16, body: String) -> Response {
    Response::text(status, body, "text/html; charset=utf-8")
        .with_header("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'")
        .with_header("X-Frame-Options", "DENY")
        .with_header("Referrer-Policy", "no-referrer")
        .with_header("Cache-Control", "no-store")
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem;color:#222}\
h1{font-size:1.3rem}.box{border:1px solid #ccc;border-radius:8px;padding:1rem;margin:1rem 0}\
label{display:block;margin:.7rem 0 .2rem}input[type=text],input[type=password]{width:100%;padding:.4rem;box-sizing:border-box}\
.err{color:#a00}.warn{background:#fff6dd;padding:.6rem;border-radius:6px}button{padding:.5rem 1rem;margin:1rem .5rem 0 0}\
code{word-break:break-all}";

fn error_page(status: u16, why: &str) -> Response {
    html(
        status,
        format!(
            "<!doctype html><html><head><meta charset=utf-8><title>GraphBuff</title><style>{STYLE}</style></head>\
             <body><h1>GraphBuff cannot connect this app</h1><p class=err>{}</p></body></html>",
            escape(why)
        ),
    )
}

/// The host of a URL, for the page.
fn host_of(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

fn consent_page(id: &str, p: &Pending, error: Option<&str>) -> String {
    let local = loopback(&p.redirect_uri).is_some();
    let warn = if local {
        "<p class=warn>This app runs on your computer: it will receive the access, through \
         <code>localhost</code>. Allow only if you started it.</p>"
    } else {
        ""
    };
    let error = error.map(|e| format!("<p class=err>{}</p>", escape(e))).unwrap_or_default();
    let (read_checked, write_checked) = if p.wants_write { ("", " checked") } else { (" checked", "") };
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Connect to GraphBuff</title><style>{STYLE}</style></head><body>\
         <h1>Connect an app to GraphBuff</h1>\
         <div class=box><b>{name}</b><br>client <code>{client}</code><br>will be sent back to <b>{host}</b></div>{warn}{error}\
         <form method=post action=/oauth/authorize>\
         <input type=hidden name=request value=\"{id}\"><input type=hidden name=csrf value=\"{csrf}\">\
         <label for=u>User</label><input type=text id=u name=user autocomplete=username required autofocus>\
         <label for=pw>Password</label><input type=password id=pw name=password autocomplete=current-password required>\
         <label>Access</label>\
         <label><input type=radio name=access value=read{read_checked}> Read only</label>\
         <label><input type=radio name=access value=write{write_checked}> Read and write (as your role allows; never administration)</label>\
         <label for=d>Only these databases (comma-separated; empty: all you may use)</label><input type=text id=d name=databases>\
         <button type=submit name=action value=allow>Allow</button><button type=submit name=action value=deny formnovalidate>Deny</button>\
         </form><p>The app gets a token that works for an hour and is renewed while you use it. \
         <code>SHOW TOKENS</code> lists it, and <code>DROP TOKEN</code> takes the access away.</p></body></html>",
        name = escape(&p.client.name),
        client = escape(&p.client.id),
        host = escape(host_of(&p.redirect_uri)),
        csrf = escape(&p.csrf),
        id = escape(id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects() {
        assert!(check_redirect("https://claude.ai/api/mcp/auth_callback").is_ok());
        assert!(check_redirect("http://localhost:3000/cb").is_ok());
        assert!(check_redirect("http://127.0.0.1/cb").is_ok());
        assert!(check_redirect("http://[::1]:9/cb").is_ok());
        assert!(check_redirect("http://example.com/cb").is_err());
        assert!(check_redirect("http://localhost.evil.com/cb").is_err());
        assert!(check_redirect("https://a/cb#x").is_err());
        let reg = vec!["http://localhost/callback".to_string(), "https://app.example/cb".to_string()];
        assert!(redirect_matches(&reg, "http://localhost:51234/callback"));
        assert!(!redirect_matches(&reg, "http://localhost:51234/other"));
        assert!(redirect_matches(&reg, "https://app.example/cb"));
        assert!(!redirect_matches(&reg, "https://app.example/cb/x"));
        assert!(!redirect_matches(&reg, "https://app.example:444/cb"));
    }

    #[test]
    fn pkce() {
        // RFC 7636, appendix B.
        assert!(pkce_ok("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!pkce_ok("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXx", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!pkce_ok("short", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
    }

    #[test]
    fn settings() {
        assert_eq!(check_public_url("https://mcp.example.com/mcp").unwrap(), "https://mcp.example.com");
        assert!(check_public_url("http://mcp.example.com/mcp").is_err());
        assert!(check_public_url("https://mcp.example.com/x/mcp").is_err());
        let c = OAuthClient::parse_list("app https://a.example/cb http://localhost/cb; cli http://127.0.0.1/x").unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].redirect_uris.len(), 2);
        assert!(OAuthClient::parse_list("app").is_err());
        assert!(OAuthClient::parse_list("app http://evil/cb").is_err());
    }

    #[test]
    fn documents() {
        let url = "https://app.example/client.json";
        let ok = r#"{"client_id":"https://app.example/client.json","client_name":"App","redirect_uris":["https://app.example/cb"],"token_endpoint_auth_method":"none"}"#;
        assert_eq!(parse_document(url, ok).unwrap().name, "App");
        assert!(parse_document("https://other.example/client.json", ok).is_err());
        assert!(parse_document(url, &ok.replace("\"none\"", "\"client_secret_basic\"")).is_err());
        assert!(parse_document(url, &ok.replace("https://app.example/cb", "http://app.example/cb")).is_err());
        assert!(parse_document(url, "[]").is_err());
    }

    #[test]
    fn page_is_escaped() {
        let p = Pending {
            client: Client { id: "https://x/<zz>".into(), name: "<script>".into(), redirect_uris: vec![] },
            redirect_uri: "https://evil\"/cb".into(),
            state: None,
            challenge: String::new(),
            wants_write: false,
            csrf: "c".into(),
            created: Instant::now(),
        };
        let page = consent_page("id", &p, Some("<qq>"));
        assert!(!page.contains("<script>") && !page.contains("<zz>") && !page.contains("<qq>") && !page.contains("evil\""));
    }
}
