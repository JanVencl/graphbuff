//! The MCP tools: their list (fixed order, so that clients' prompt caches hold) and what
//! each one does.

use std::collections::BTreeMap;

use crate::api::Ctx;
use crate::json::{self, Json};
use crate::users::{Access, Principal};

use super::rpc::{encode, obj, string};

/// What a tool is called with.
pub(crate) struct Input<'a> {
    pub ctx: &'a Ctx,
    pub who: &'a Principal,
    pub args: &'a BTreeMap<String, Json>,
}

/// A tool's answer: text for the model, and whether it is an error the model may correct.
pub(crate) struct Output {
    pub text: String,
    pub is_error: bool,
}

impl Output {
    pub(crate) fn ok(text: String) -> Output {
        Output { text, is_error: false }
    }

    pub(crate) fn error(text: impl Into<String>) -> Output {
        Output { text: text.into(), is_error: true }
    }
}

pub(crate) struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// The JSON Schema of the arguments.
    pub schema: &'static str,
    /// It changes data: only logins that may write see it.
    pub writes: bool,
    pub idempotent: bool,
    pub run: fn(&Input) -> Output,
}

/// Every tool, in the order `tools/list` gives them.
pub(crate) const TOOLS: &[Tool] = &[Tool {
    name: "list_databases",
    title: "List databases",
    description: "Lists the graph databases you may use: for each its name, whether it is the default one \
                  (used when a tool is given no database), your access (read or write), and its numbers of \
                  nodes and relationships.",
    schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    writes: false,
    idempotent: true,
    run: list_databases,
}];

/// The tools a login with access `best` (its strongest on any database, capped for MCP)
/// sees.
pub(crate) fn visible(best: Access) -> impl Iterator<Item = &'static Tool> {
    TOOLS.iter().filter(move |t| !t.writes || best >= Access::Write)
}

/// The `tools` array of `tools/list`.
pub(crate) fn list(best: Access) -> Json {
    Json::Array(
        visible(best)
            .map(|t| {
                let schema = json::parse(t.schema).expect("tool schemas are valid JSON");
                obj([
                    ("name", string(t.name)),
                    ("title", string(t.title)),
                    ("description", string(t.description)),
                    ("inputSchema", schema),
                    (
                        "annotations",
                        obj([
                            ("readOnlyHint", Json::Bool(!t.writes)),
                            ("destructiveHint", Json::Bool(t.writes)),
                            ("idempotentHint", Json::Bool(t.idempotent)),
                            ("openWorldHint", Json::Bool(false)),
                        ]),
                    ),
                ])
            })
            .collect(),
    )
}

/// An error for the first argument a tool without arguments got.
fn no_arguments(tool: &str, args: &BTreeMap<String, Json>) -> Option<Output> {
    args.keys().next().map(|k| Output::error(format!("{tool} takes no arguments, but got `{k}`")))
}

fn list_databases(i: &Input) -> Output {
    if let Some(e) = no_arguments("list_databases", i.args) {
        return e;
    }
    let mut dbs = Vec::new();
    for d in i.ctx.mgr.databases() {
        let access = super::cap(i.ctx.access_on(i.who, &d.name));
        if access < Access::Read {
            continue;
        }
        let access = if access >= Access::Write && !d.read_only { "write" } else { "read" };
        dbs.push(obj([
            ("name", string(d.name)),
            ("default", Json::Bool(d.default)),
            ("access", string(access)),
            ("nodes", Json::Int(d.nodes as i64)),
            ("relationships", Json::Int(d.relationships as i64)),
        ]));
    }
    Output::ok(encode(&obj([("databases", Json::Array(dbs))])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_parse_and_names_are_unique() {
        for (i, t) in TOOLS.iter().enumerate() {
            assert!(matches!(json::parse(t.schema), Ok(Json::Object(_))), "{}", t.name);
            assert!(TOOLS[..i].iter().all(|u| u.name != t.name), "{}", t.name);
            assert!(t.name.len() <= 64 && t.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
        assert_eq!(visible(Access::Read).count(), TOOLS.iter().filter(|t| !t.writes).count());
    }
}
