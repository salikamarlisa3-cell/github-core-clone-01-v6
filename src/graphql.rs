//! A compact GraphQL implementation for the fields the forge exposes:
//! lexer, parser (operations, fragments, variables, directives), and a
//! selection-set executor over typed resolver objects. Global ids are
//! base64 of `Type:key`, stable across surfaces; unknown fields produce
//! errors that name the field, as the suite's invalid-field check wants.

use std::collections::HashMap;

use crate::json::Json;
use crate::{store, Request, Response};

// ------------------------------------------------------------------- lexer

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Name(String),
    Int(i64),
    Float(f64),
    Str(String),
    Punct(char),
    Spread,
    Eof,
}

fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let bytes = input.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            ' ' | '\t' | '\n' | '\r' | ',' => i += 1,
            '#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '.' => {
                if input[i..].starts_with("...") {
                    toks.push(Tok::Spread);
                    i += 3;
                } else {
                    return Err("stray dot".to_string());
                }
            }
            '{' | '}' | '(' | ')' | '[' | ']' | ':' | '=' | '@' | '$' | '!' | '|' | '&' => {
                toks.push(Tok::Punct(c));
                i += 1;
            }
            '"' => {
                if input[i..].starts_with("\"\"\"") {
                    let end = input[i + 3..].find("\"\"\"").ok_or("unterminated block string")?;
                    toks.push(Tok::Str(input[i + 3..i + 3 + end].to_string()));
                    i += 3 + end + 3;
                } else {
                    // Reuse the JSON string parser for escapes.
                    let mut j = i + 1;
                    let mut raw = String::from("\"");
                    loop {
                        if j >= bytes.len() {
                            return Err("unterminated string".to_string());
                        }
                        raw.push(bytes[j] as char);
                        if bytes[j] == b'\\' {
                            j += 1;
                            if j < bytes.len() {
                                raw.push(bytes[j] as char);
                            }
                        } else if bytes[j] == b'"' {
                            break;
                        }
                        j += 1;
                    }
                    match Json::parse(raw.as_bytes()) {
                        Ok(Json::Str(s)) => toks.push(Tok::Str(s)),
                        _ => return Err("bad string literal".to_string()),
                    }
                    i = j + 1;
                }
            }
            '-' | '0'..='9' => {
                let start = i;
                if bytes[i] == b'-' {
                    i += 1;
                }
                while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                    i += 1;
                }
                let mut is_float = false;
                if i < bytes.len() && bytes[i] == b'.' {
                    is_float = true;
                    i += 1;
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                    is_float = true;
                    i += 1;
                    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                        i += 1;
                    }
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                }
                let text = &input[start..i];
                if is_float {
                    toks.push(Tok::Float(text.parse().map_err(|_| "bad float")?));
                } else {
                    toks.push(Tok::Int(text.parse().map_err(|_| "bad int")?));
                }
            }
            _ if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len() {
                    let ch = bytes[i] as char;
                    if ch.is_ascii_alphanumeric() || ch == '_' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                toks.push(Tok::Name(input[start..i].to_string()));
            }
            _ => return Err(format!("unexpected character {c:?}")),
        }
    }
    toks.push(Tok::Eof);
    Ok(toks)
}

// ------------------------------------------------------------------ parser

#[derive(Clone, Debug)]
enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Enum(String),
    Var(String),
    List(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

#[derive(Clone, Debug)]
struct Field {
    alias: Option<String>,
    name: String,
    args: Vec<(String, Value)>,
    directives: Vec<(String, Vec<(String, Value)>)>,
    selections: Vec<Selection>,
}

#[derive(Clone, Debug)]
enum Selection {
    Field(Field),
    Spread(String, Vec<(String, Vec<(String, Value)>)>),
    Inline(Option<String>, Vec<Selection>),
}

#[derive(Clone, Debug)]
enum Def {
    Operation {
        kind: String, // "query" | "mutation" | "subscription"
        name: Option<String>,
        var_defaults: Vec<(String, Value)>,
        selections: Vec<Selection>,
    },
    Fragment {
        name: String,
        type_cond: String,
        selections: Vec<Selection>,
    },
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Tok {
        let tok = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        tok
    }

    fn expect_punct(&mut self, c: char) -> Result<(), String> {
        if self.next() == Tok::Punct(c) {
            Ok(())
        } else {
            Err(format!("expected {c}"))
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Name(n) => Ok(n),
            _ => Err("expected a name".to_string()),
        }
    }

    fn document(&mut self) -> Result<Vec<Def>, String> {
        let mut defs = Vec::new();
        while *self.peek() != Tok::Eof {
            defs.push(self.definition()?);
        }
        Ok(defs)
    }

    fn definition(&mut self) -> Result<Def, String> {
        match self.peek().clone() {
            Tok::Punct('{') => Ok(Def::Operation {
                kind: "query".to_string(),
                name: None,
                var_defaults: vec![],
                selections: self.selection_set()?,
            }),
            Tok::Name(word) => match word.as_str() {
                "query" | "mutation" | "subscription" => {
                    self.next();
                    let name = match self.peek() {
                        Tok::Name(_) => Some(self.name()?),
                        _ => None,
                    };
                    let mut var_defaults = Vec::new();
                    if *self.peek() == Tok::Punct('(') {
                        self.next();
                        while *self.peek() != Tok::Punct(')') {
                            self.expect_punct('$')?;
                            let var = self.name()?;
                            self.expect_punct(':')?;
                            self.type_ref()?;
                            if *self.peek() == Tok::Punct('=') {
                                self.next();
                                var_defaults.push((var, self.value()?));
                            }
                            self.directives()?;
                        }
                        self.next();
                    }
                    self.directives()?;
                    let selections = self.selection_set()?;
                    Ok(Def::Operation { kind: word, name, var_defaults, selections })
                }
                "fragment" => {
                    self.next();
                    let frag_name = self.name()?;
                    let on = self.name()?;
                    if on != "on" {
                        return Err("fragment needs 'on'".to_string());
                    }
                    let type_cond = self.name()?;
                    self.directives()?;
                    let selections = self.selection_set()?;
                    Ok(Def::Fragment { name: frag_name, type_cond, selections })
                }
                _ => Err(format!("unexpected definition {word}")),
            },
            _ => Err("unexpected token in document".to_string()),
        }
    }

    /// Skip a type reference like `[String!]!`.
    fn type_ref(&mut self) -> Result<(), String> {
        match self.next() {
            Tok::Punct('[') => {
                self.type_ref()?;
                self.expect_punct(']')?;
            }
            Tok::Name(_) => {}
            _ => return Err("bad type reference".to_string()),
        }
        if *self.peek() == Tok::Punct('!') {
            self.next();
        }
        Ok(())
    }

    fn directives(&mut self) -> Result<Vec<(String, Vec<(String, Value)>)>, String> {
        let mut out = Vec::new();
        while *self.peek() == Tok::Punct('@') {
            self.next();
            let name = self.name()?;
            let args = if *self.peek() == Tok::Punct('(') {
                self.arguments()?
            } else {
                Vec::new()
            };
            out.push((name, args));
        }
        Ok(out)
    }

    fn arguments(&mut self) -> Result<Vec<(String, Value)>, String> {
        self.expect_punct('(')?;
        let mut args = Vec::new();
        while *self.peek() != Tok::Punct(')') {
            let name = self.name()?;
            self.expect_punct(':')?;
            let value = self.value()?;
            args.push((name, value));
        }
        self.next();
        Ok(args)
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.next() {
            Tok::Int(n) => Ok(Value::Int(n)),
            Tok::Float(n) => Ok(Value::Float(n)),
            Tok::Str(s) => Ok(Value::Str(s)),
            Tok::Name(n) => Ok(match n.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                _ => Value::Enum(n),
            }),
            Tok::Punct('$') => Ok(Value::Var(self.name()?)),
            Tok::Punct('[') => {
                let mut items = Vec::new();
                while *self.peek() != Tok::Punct(']') {
                    items.push(self.value()?);
                }
                self.next();
                Ok(Value::List(items))
            }
            Tok::Punct('{') => {
                let mut fields = Vec::new();
                while *self.peek() != Tok::Punct('}') {
                    let name = self.name()?;
                    self.expect_punct(':')?;
                    fields.push((name, self.value()?));
                }
                self.next();
                Ok(Value::Obj(fields))
            }
            _ => Err("bad value".to_string()),
        }
    }

    fn selection_set(&mut self) -> Result<Vec<Selection>, String> {
        self.expect_punct('{')?;
        let mut selections = Vec::new();
        while *self.peek() != Tok::Punct('}') {
            selections.push(self.selection()?);
        }
        self.next();
        Ok(selections)
    }

    fn selection(&mut self) -> Result<Selection, String> {
        if *self.peek() == Tok::Spread {
            self.next();
            if *self.peek() == Tok::Name("on".to_string()) {
                self.next();
                let type_cond = self.name()?;
                self.directives()?;
                let selections = self.selection_set()?;
                return Ok(Selection::Inline(Some(type_cond), selections));
            }
            match self.peek().clone() {
                Tok::Name(name) => {
                    self.next();
                    let dirs = self.directives()?;
                    Ok(Selection::Spread(name, dirs))
                }
                _ => {
                    let selections = self.selection_set()?;
                    Ok(Selection::Inline(None, selections))
                }
            }
        } else {
            let mut name = self.name()?;
            let mut alias = None;
            if *self.peek() == Tok::Punct(':') {
                self.next();
                alias = Some(name);
                name = self.name()?;
            }
            let args = if *self.peek() == Tok::Punct('(') {
                self.arguments()?
            } else {
                Vec::new()
            };
            let directives = self.directives()?;
            let selections = if *self.peek() == Tok::Punct('{') {
                self.selection_set()?
            } else {
                Vec::new()
            };
            Ok(Selection::Field(Field { alias, name, args, directives, selections }))
        }
    }
}

// ---------------------------------------------------------------- executor

#[derive(Clone)]
enum GVal {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<GVal>),
    Obj(GObj),
}

#[derive(Clone)]
struct GObj {
    type_name: &'static str,
    data: ObjData,
}

#[derive(Clone)]
enum ObjData {
    Map(Vec<(String, GVal)>),
    Repo(i64),
    Issue(i64, i64),   // repository db id, issue number
    Pull(i64, i64),    // repository db id, pull number
    Comment(i64, i64, i64), // repository db id, issue number, comment id
    Label(i64, String),     // repository db id, label name
}

impl GVal {
    fn obj(type_name: &'static str, fields: Vec<(&str, GVal)>) -> GVal {
        GVal::Obj(GObj {
            type_name,
            data: ObjData::Map(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
        })
    }
}

fn gid(type_name: &str, key: &str) -> String {
    crate::store::b64_encode(format!("{type_name}:{key}").as_bytes())
}

fn parse_gid(id: &str) -> Option<(String, String)> {
    let raw = crate::store::b64_decode(id)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.split_once(':').map(|(a, b)| (a.to_string(), b.to_string()))
}

struct Ctx<'a> {
    st: &'a mut crate::store::Store,
    vars: HashMap<String, Json>,
    fragments: HashMap<String, (String, Vec<Selection>)>,
    errors: Vec<String>,
}

fn eval(value: &Value, vars: &HashMap<String, Json>) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::Num(*n as f64),
        Value::Float(n) => Json::Num(*n),
        Value::Str(s) | Value::Enum(s) => Json::Str(s.clone()),
        Value::Var(name) => vars.get(name).cloned().unwrap_or(Json::Null),
        Value::List(items) => Json::Arr(items.iter().map(|i| eval(i, vars)).collect()),
        Value::Obj(fields) => {
            Json::Obj(fields.iter().map(|(k, v)| (k.clone(), eval(v, vars))).collect())
        }
    }
}

struct Args(HashMap<String, Json>);

impl Args {
    fn of(field: &Field, vars: &HashMap<String, Json>) -> Args {
        Args(field.args.iter().map(|(k, v)| (k.clone(), eval(v, vars))).collect())
    }

    fn str_of(&self, name: &str) -> Option<String> {
        self.0.get(name).and_then(|v| v.as_str()).map(|s| s.to_string())
    }

    fn int_of(&self, name: &str) -> Option<i64> {
        self.0.get(name).and_then(|v| v.as_i64())
    }

    fn obj_get<'a>(&'a self, name: &str, key: &str) -> Option<&'a Json> {
        self.0.get(name).and_then(|v| v.get(key))
    }
}

fn directives_allow(directives: &[(String, Vec<(String, Value)>)], vars: &HashMap<String, Json>) -> bool {
    for (name, args) in directives {
        let if_value = args
            .iter()
            .find(|(k, _)| k == "if")
            .map(|(_, v)| eval(v, vars))
            .and_then(|v| v.as_bool());
        match (name.as_str(), if_value) {
            ("skip", Some(true)) => return false,
            ("include", Some(false)) => return false,
            _ => {}
        }
    }
    true
}

fn type_matches(cond: &str, actual: &str) -> bool {
    cond == actual || cond == "Node"
}

fn project(ctx: &mut Ctx, value: &GVal, selections: &[Selection]) -> Json {
    match value {
        GVal::Null => Json::Null,
        GVal::Bool(b) => Json::Bool(*b),
        GVal::Int(n) => Json::Num(*n as f64),
        GVal::Str(s) => Json::Str(s.clone()),
        GVal::List(items) => {
            Json::Arr(items.iter().map(|item| project(ctx, item, selections)).collect())
        }
        GVal::Obj(obj) => {
            if selections.is_empty() {
                // No sub-selection: serialize a static map wholesale.
                if let ObjData::Map(fields) = &obj.data {
                    let rendered = fields
                        .iter()
                        .map(|(k, v)| (k.clone(), project(ctx, v, &[])))
                        .collect();
                    return Json::Obj(rendered);
                }
                return Json::Null;
            }
            let mut out: Vec<(String, Json)> = Vec::new();
            project_into(ctx, obj, selections, &mut out);
            Json::Obj(out)
        }
    }
}

fn project_into(ctx: &mut Ctx, obj: &GObj, selections: &[Selection], out: &mut Vec<(String, Json)>) {
    for selection in selections {
        match selection {
            Selection::Field(field) => {
                if !directives_allow(&field.directives, &ctx.vars) {
                    continue;
                }
                let key = field.alias.clone().unwrap_or_else(|| field.name.clone());
                if field.name == "__typename" {
                    out.push((key, Json::Str(obj.type_name.to_string())));
                    continue;
                }
                let args = Args::of(field, &ctx.vars);
                match resolve_field(ctx, obj, &field.name, &args) {
                    Ok(value) => {
                        let rendered = project(ctx, &value, &field.selections);
                        out.push((key, rendered));
                    }
                    Err(message) => {
                        ctx.errors.push(message);
                        out.push((key, Json::Null));
                    }
                }
            }
            Selection::Spread(name, directives) => {
                if !directives_allow(directives, &ctx.vars) {
                    continue;
                }
                if let Some((cond, frag_selections)) = ctx.fragments.get(name).cloned() {
                    if type_matches(&cond, obj.type_name) {
                        project_into(ctx, obj, &frag_selections, out);
                    }
                }
            }
            Selection::Inline(cond, inner) => {
                let applies = match cond {
                    Some(c) => type_matches(c, obj.type_name),
                    None => true,
                };
                if applies {
                    project_into(ctx, obj, inner, out);
                }
            }
        }
    }
}

fn field_error(type_name: &str, field: &str) -> String {
    format!("Field '{field}' doesn't exist on type '{type_name}'")
}

// ------------------------------------------------------------- object data

fn user_obj(login: &str) -> GVal {
    let (name, email) = if login == crate::store::VIEWER_LOGIN {
        (crate::store::VIEWER_NAME, crate::store::VIEWER_EMAIL)
    } else {
        (login, "")
    };
    GVal::obj(
        "User",
        vec![
            ("id", GVal::Str(gid("User", login))),
            ("login", GVal::Str(login.to_string())),
            ("name", GVal::Str(name.to_string())),
            ("email", GVal::Str(email.to_string())),
            ("url", GVal::Str(format!("https://github.com/{login}"))),
        ],
    )
}

fn page_info(has_next: bool, has_prev: bool, start: Option<String>, end: Option<String>) -> GVal {
    GVal::obj(
        "PageInfo",
        vec![
            ("hasNextPage", GVal::Bool(has_next)),
            ("hasPreviousPage", GVal::Bool(has_prev)),
            (
                "startCursor",
                start.map(GVal::Str).unwrap_or(GVal::Null),
            ),
            ("endCursor", end.map(GVal::Str).unwrap_or(GVal::Null)),
        ],
    )
}

fn cursor_of(index: usize) -> String {
    crate::store::b64_encode(format!("cursor:{index}").as_bytes())
}

fn cursor_index(cursor: &str) -> Option<usize> {
    let raw = crate::store::b64_decode(cursor)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.strip_prefix("cursor:")?.parse().ok()
}

/// Slice `items` (already in final order) by first/after and wrap them
/// in a connection object of the given type name.
fn connection(type_name: &'static str, items: Vec<GVal>, args: &Args) -> GVal {
    let total = items.len();
    let start = args
        .str_of("after")
        .and_then(|c| cursor_index(&c))
        .map(|i| (i + 1).min(total))
        .unwrap_or(0);
    let first = args.int_of("first").map(|n| n.max(0) as usize).unwrap_or(total);
    let end = (start + first).min(total);
    let page_items: Vec<GVal> = items[start..end].to_vec();
    let edges: Vec<GVal> = page_items
        .iter()
        .enumerate()
        .map(|(i, node)| {
            GVal::obj(
                "Edge",
                vec![
                    ("node", node.clone()),
                    ("cursor", GVal::Str(cursor_of(start + i))),
                ],
            )
        })
        .collect();
    GVal::obj(
        type_name,
        vec![
            ("edges", GVal::List(edges)),
            ("nodes", GVal::List(page_items.clone())),
            (
                "pageInfo",
                page_info(
                    end < total,
                    start > 0,
                    if page_items.is_empty() { None } else { Some(cursor_of(start)) },
                    if page_items.is_empty() { None } else { Some(cursor_of(end - 1)) },
                ),
            ),
            ("totalCount", GVal::Int(total as i64)),
        ],
    )
}

fn order_direction(args: &Args) -> Option<String> {
    args.obj_get("orderBy", "direction").and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn state_filter(args: &Args) -> Option<Vec<String>> {
    args.0.get("states").and_then(|v| v.as_arr()).map(|items| {
        items.iter().filter_map(|i| i.as_str().map(|s| s.to_string())).collect()
    })
}

// --------------------------------------------------------------- resolvers

fn repo_by_id<'a>(st: &'a crate::store::Store, id: i64) -> Option<&'a crate::store::Repo> {
    st.repos.values().find(|r| r.id == id)
}

fn resolve_field(ctx: &mut Ctx, obj: &GObj, field: &str, args: &Args) -> Result<GVal, String> {
    // Static maps answer from their own fields (payloads, connections).
    if let ObjData::Map(fields) = &obj.data {
        if obj.type_name != "Query" && obj.type_name != "Mutation" {
            return match fields.iter().find(|(k, _)| k == field) {
                Some((_, value)) => Ok(value.clone()),
                None => Err(field_error(obj.type_name, field)),
            };
        }
    }
    match obj.type_name {
        "Query" => resolve_query(ctx, field, args),
        "Mutation" => resolve_mutation(ctx, field, args),
        "Repository" => match &obj.data {
            ObjData::Repo(id) => resolve_repository(ctx, *id, field, args),
            _ => Err(field_error("Repository", field)),
        },
        "Issue" => match &obj.data {
            ObjData::Issue(repo_id, number) => resolve_issue(ctx, *repo_id, *number, field, args),
            _ => Err(field_error("Issue", field)),
        },
        "PullRequest" => match &obj.data {
            ObjData::Pull(repo_id, number) => resolve_pull(ctx, *repo_id, *number, field, args),
            _ => Err(field_error("PullRequest", field)),
        },
        "IssueComment" => match &obj.data {
            ObjData::Comment(repo_id, issue_number, comment_id) => {
                resolve_comment(ctx, *repo_id, *issue_number, *comment_id, field)
            }
            _ => Err(field_error("IssueComment", field)),
        },
        "Label" => match &obj.data {
            ObjData::Label(repo_id, name) => resolve_label(ctx, *repo_id, name, field),
            _ => Err(field_error("Label", field)),
        },
        _ => Err(field_error(obj.type_name, field)),
    }
}

fn resolve_query(ctx: &mut Ctx, field: &str, args: &Args) -> Result<GVal, String> {
    match field {
        "viewer" => Ok(user_obj(crate::store::VIEWER_LOGIN)),
        "repository" => {
            let owner = args.str_of("owner").unwrap_or_default();
            let name = args.str_of("name").unwrap_or_default();
            match ctx.st.repo(&owner, &name) {
                Some(repo) => Ok(GVal::Obj(GObj {
                    type_name: "Repository",
                    data: ObjData::Repo(repo.id),
                })),
                None => Ok(GVal::Null),
            }
        }
        "node" => {
            let id = args.str_of("id").unwrap_or_default();
            Ok(node_by_gid(ctx, &id))
        }
        "__type" => {
            let name = args.str_of("name").unwrap_or_default();
            Ok(type_object(&name))
        }
        "__schema" => Ok(GVal::obj(
            "Schema",
            vec![
                ("queryType", GVal::obj("Type", vec![("name", GVal::Str("Query".to_string()))])),
                (
                    "mutationType",
                    GVal::obj("Type", vec![("name", GVal::Str("Mutation".to_string()))]),
                ),
            ],
        )),
        "search" => Ok(connection(
            "SearchResultItemConnection",
            vec![],
            args,
        )),
        _ => Err(field_error("Query", field)),
    }
}

fn node_by_gid(ctx: &mut Ctx, id: &str) -> GVal {
    let (kind, key) = match parse_gid(id) {
        Some(v) => v,
        None => return GVal::Null,
    };
    match kind.as_str() {
        "Repository" => match key.parse::<i64>().ok().and_then(|dbid| repo_by_id(ctx.st, dbid)) {
            Some(repo) => GVal::Obj(GObj { type_name: "Repository", data: ObjData::Repo(repo.id) }),
            None => GVal::Null,
        },
        "Issue" | "PullRequest" => {
            let mut parts = key.split(':');
            let dbid: i64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(-1);
            let number: i64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(-1);
            let repo = match repo_by_id(ctx.st, dbid) {
                Some(r) => r,
                None => return GVal::Null,
            };
            if kind == "Issue" && repo.issue(number).is_some() {
                GVal::Obj(GObj { type_name: "Issue", data: ObjData::Issue(repo.id, number) })
            } else if kind == "PullRequest" && repo.pull(number).is_some() {
                GVal::Obj(GObj { type_name: "PullRequest", data: ObjData::Pull(repo.id, number) })
            } else {
                GVal::Null
            }
        }
        "User" => user_obj(&key),
        _ => GVal::Null,
    }
}

fn resolve_repository(ctx: &mut Ctx, id: i64, field: &str, args: &Args) -> Result<GVal, String> {
    let repo = repo_by_id(ctx.st, id).ok_or_else(|| field_error("Repository", field))?;
    let value = match field {
        "id" => GVal::Str(gid("Repository", &id.to_string())),
        "databaseId" => GVal::Int(id),
        "name" => GVal::Str(repo.name.clone()),
        "nameWithOwner" => GVal::Str(repo.full_name()),
        "owner" => user_obj(&repo.owner),
        "description" => {
            if repo.description.is_empty() {
                GVal::Null
            } else {
                GVal::Str(repo.description.clone())
            }
        }
        "isPrivate" => GVal::Bool(repo.private),
        "viewerPermission" => {
            GVal::Str(if repo.owner == crate::store::VIEWER_LOGIN { "ADMIN" } else { "READ" }.to_string())
        }
        "hasIssuesEnabled" => GVal::Bool(true),
        "hasPullRequestsEnabled" => GVal::Bool(true),
        "isEmpty" => GVal::Bool(repo.refs.is_empty()),
        "url" => GVal::Str(format!("https://github.com/{}", repo.full_name())),
        "createdAt" => GVal::Str(crate::store::iso8601(repo.created)),
        "updatedAt" => GVal::Str(crate::store::iso8601(repo.created)),
        "defaultBranchRef" => match repo.head_oid() {
            Some(tip) => GVal::obj(
                "Ref",
                vec![
                    ("name", GVal::Str(repo.default_branch.clone())),
                    (
                        "target",
                        GVal::obj(
                            "Commit",
                            vec![("oid", GVal::Str(crate::sha1::oid_hex(&tip)))],
                        ),
                    ),
                ],
            ),
            None => GVal::Null,
        },
        "issue" => {
            let number = args.int_of("number").unwrap_or(0);
            if repo.issue(number).is_some() {
                GVal::Obj(GObj { type_name: "Issue", data: ObjData::Issue(id, number) })
            } else {
                GVal::Null
            }
        }
        "issues" => {
            let states = state_filter(args);
            let mut numbers: Vec<i64> = repo
                .issues
                .iter()
                .filter(|i| match &states {
                    Some(list) => list.iter().any(|s| s.eq_ignore_ascii_case(&i.state)),
                    None => true,
                })
                .map(|i| i.number)
                .collect();
            numbers.sort();
            if order_direction(args).as_deref() == Some("DESC") {
                numbers.reverse();
            }
            let items: Vec<GVal> = numbers
                .into_iter()
                .map(|n| GVal::Obj(GObj { type_name: "Issue", data: ObjData::Issue(id, n) }))
                .collect();
            connection("IssueConnection", items, args)
        }
        "pullRequest" => {
            let number = args.int_of("number").unwrap_or(0);
            if repo.pull(number).is_some() {
                GVal::Obj(GObj { type_name: "PullRequest", data: ObjData::Pull(id, number) })
            } else {
                GVal::Null
            }
        }
        "pullRequests" => {
            let states = state_filter(args).unwrap_or_else(|| vec!["OPEN".to_string()]);
            let mut numbers: Vec<i64> = repo
                .pulls
                .iter()
                .filter(|p| {
                    let state = if p.merged {
                        "MERGED"
                    } else if p.state == "open" {
                        "OPEN"
                    } else {
                        "CLOSED"
                    };
                    states.iter().any(|s| s.eq_ignore_ascii_case(state))
                })
                .map(|p| p.number)
                .collect();
            numbers.sort();
            if order_direction(args).as_deref() == Some("DESC") {
                numbers.reverse();
            }
            let items: Vec<GVal> = numbers
                .into_iter()
                .map(|n| GVal::Obj(GObj { type_name: "PullRequest", data: ObjData::Pull(id, n) }))
                .collect();
            connection("PullRequestConnection", items, args)
        }
        _ => return Err(field_error("Repository", field)),
    };
    Ok(value)
}

fn resolve_issue(ctx: &mut Ctx, repo_id: i64, number: i64, field: &str, args: &Args) -> Result<GVal, String> {
    let repo = repo_by_id(ctx.st, repo_id).ok_or_else(|| field_error("Issue", field))?;
    let issue = repo.issue(number).ok_or_else(|| field_error("Issue", field))?;
    let value = match field {
        "id" => GVal::Str(gid("Issue", &format!("{repo_id}:{number}"))),
        "databaseId" => GVal::Int(issue.gid),
        "number" => GVal::Int(number),
        "title" => GVal::Str(issue.title.clone()),
        "body" => GVal::Str(issue.body.clone()),
        "state" => GVal::Str(issue.state.to_uppercase()),
        "closed" => GVal::Bool(issue.state == "closed"),
        "url" => GVal::Str(format!("https://github.com/{}/issues/{}", repo.full_name(), number)),
        "author" => user_obj(&issue.author),
        "createdAt" => GVal::Str(crate::store::iso8601(issue.created)),
        "updatedAt" => GVal::Str(crate::store::iso8601(issue.updated)),
        "repository" => GVal::Obj(GObj { type_name: "Repository", data: ObjData::Repo(repo_id) }),
        "labels" => {
            let items: Vec<GVal> = issue
                .labels
                .iter()
                .map(|name| {
                    GVal::Obj(GObj {
                        type_name: "Label",
                        data: ObjData::Label(repo_id, name.clone()),
                    })
                })
                .collect();
            connection("LabelConnection", items, args)
        }
        "comments" => {
            let items: Vec<GVal> = issue
                .comments
                .iter()
                .map(|c| {
                    GVal::Obj(GObj {
                        type_name: "IssueComment",
                        data: ObjData::Comment(repo_id, number, c.id),
                    })
                })
                .collect();
            connection("IssueCommentConnection", items, args)
        }
        _ => return Err(field_error("Issue", field)),
    };
    Ok(value)
}

fn resolve_comment(ctx: &mut Ctx, repo_id: i64, issue_number: i64, comment_id: i64, field: &str) -> Result<GVal, String> {
    let repo = repo_by_id(ctx.st, repo_id).ok_or_else(|| field_error("IssueComment", field))?;
    let comment = repo
        .issue(issue_number)
        .and_then(|i| i.comments.iter().find(|c| c.id == comment_id))
        .ok_or_else(|| field_error("IssueComment", field))?;
    let value = match field {
        "id" => GVal::Str(gid("IssueComment", &comment_id.to_string())),
        "databaseId" => GVal::Int(comment_id),
        "body" => GVal::Str(comment.body.clone()),
        "author" => user_obj(&comment.author),
        "createdAt" => GVal::Str(crate::store::iso8601(comment.created)),
        "updatedAt" => GVal::Str(crate::store::iso8601(comment.created)),
        "url" => GVal::Str(format!(
            "https://github.com/{}/issues/{}#issuecomment-{}",
            repo.full_name(),
            issue_number,
            comment_id
        )),
        _ => return Err(field_error("IssueComment", field)),
    };
    Ok(value)
}

fn resolve_label(ctx: &mut Ctx, repo_id: i64, name: &str, field: &str) -> Result<GVal, String> {
    let repo = repo_by_id(ctx.st, repo_id).ok_or_else(|| field_error("Label", field))?;
    let label = repo.label(name).ok_or_else(|| field_error("Label", field))?;
    let value = match field {
        "id" => GVal::Str(gid("Label", &format!("{repo_id}:{}", label.name))),
        "name" => GVal::Str(label.name.clone()),
        "color" => GVal::Str(label.color.clone()),
        "description" => GVal::Null,
        _ => return Err(field_error("Label", field)),
    };
    Ok(value)
}

fn pull_state(pull: &crate::store::Pull) -> &'static str {
    if pull.merged {
        "MERGED"
    } else if pull.state == "open" {
        "OPEN"
    } else {
        "CLOSED"
    }
}

fn resolve_pull(ctx: &mut Ctx, repo_id: i64, number: i64, field: &str, args: &Args) -> Result<GVal, String> {
    let repo = repo_by_id(ctx.st, repo_id).ok_or_else(|| field_error("PullRequest", field))?;
    let pull = repo.pull(number).ok_or_else(|| field_error("PullRequest", field))?;
    let value = match field {
        "id" => GVal::Str(gid("PullRequest", &format!("{repo_id}:{number}"))),
        "databaseId" => GVal::Int(pull.gid),
        "number" => GVal::Int(number),
        "title" => GVal::Str(pull.title.clone()),
        "body" => GVal::Str(pull.body.clone()),
        "state" => GVal::Str(pull_state(pull).to_string()),
        "merged" => GVal::Bool(pull.merged),
        "isDraft" => GVal::Bool(false),
        "url" => GVal::Str(format!("https://github.com/{}/pulls/{}", repo.full_name(), number)),
        "author" => user_obj(&pull.author),
        "createdAt" => GVal::Str(crate::store::iso8601(pull.created)),
        "updatedAt" => GVal::Str(crate::store::iso8601(pull.updated)),
        "headRefName" => GVal::Str(pull.head_ref.clone()),
        "baseRefName" => GVal::Str(pull.base_ref.clone()),
        "headRefOid" => GVal::Str(
            repo.refs
                .get(&format!("refs/heads/{}", pull.head_ref))
                .map(|o| crate::sha1::oid_hex(o))
                .unwrap_or_default(),
        ),
        "baseRefOid" => GVal::Str(
            repo.refs
                .get(&format!("refs/heads/{}", pull.base_ref))
                .map(|o| crate::sha1::oid_hex(o))
                .unwrap_or_default(),
        ),
        "mergedAt" => pull
            .merged_at
            .map(|t| GVal::Str(crate::store::iso8601(t)))
            .unwrap_or(GVal::Null),
        "mergeCommit" => match &pull.merge_commit {
            Some(sha) => GVal::obj("Commit", vec![("oid", GVal::Str(sha.clone()))]),
            None => GVal::Null,
        },
        "mergeable" => GVal::Str(mergeable_state(ctx, repo_id, number).to_string()),
        "repository" => GVal::Obj(GObj { type_name: "Repository", data: ObjData::Repo(repo_id) }),
        "files" => {
            let items: Vec<GVal> = pull_file_nodes(repo, pull).into_iter().map(|f| f).collect();
            connection("PullRequestChangedFileConnection", items, args)
        }
        "comments" => connection("IssueCommentConnection", vec![], args),
        _ => return Err(field_error("PullRequest", field)),
    };
    Ok(value)
}

/// The GraphQL view of a pull request's changed files.
fn pull_file_nodes(repo: &crate::store::Repo, pull: &crate::store::Pull) -> Vec<GVal> {
    let (Some(base_tip), Some(head_tip)) = (
        repo.refs.get(&format!("refs/heads/{}", pull.base_ref)).copied(),
        repo.refs.get(&format!("refs/heads/{}", pull.head_ref)).copied(),
    ) else {
        return Vec::new();
    };
    let head_commit = match repo.objects.get(&head_tip).and_then(|o| crate::gitcore::parse_commit(&o.data)) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let head_map = crate::gitcore::flatten_tree(repo, &head_commit.tree);
    let origin_map = match crate::gitcore::merge_base(repo, &base_tip, &head_tip) {
        Some(mb) => match repo.objects.get(&mb).and_then(|o| crate::gitcore::parse_commit(&o.data)) {
            Some(c) => crate::gitcore::flatten_tree(repo, &c.tree),
            None => Default::default(),
        },
        None => Default::default(),
    };
    crate::gitcore::diff_maps(&origin_map, &head_map)
        .iter()
        .map(|change| {
            let old_bytes = change.old.and_then(|oid| repo.objects.get(&oid)).map(|o| (*o.data).clone()).unwrap_or_default();
            let new_bytes = change.new.and_then(|oid| repo.objects.get(&oid)).map(|o| (*o.data).clone()).unwrap_or_default();
            let (additions, deletions) = crate::gitcore::line_diff(&old_bytes, &new_bytes);
            let change_type = match change.status {
                "added" => "ADDED",
                "removed" => "DELETED",
                _ => "MODIFIED",
            };
            GVal::obj(
                "PullRequestChangedFile",
                vec![
                    ("path", GVal::Str(change.path.clone())),
                    ("additions", GVal::Int(additions as i64)),
                    ("deletions", GVal::Int(deletions as i64)),
                    ("changeType", GVal::Str(change_type.to_string())),
                ],
            )
        })
        .collect()
}

fn mergeable_state(ctx: &mut Ctx, repo_id: i64, number: i64) -> &'static str {
    let repo = match repo_by_id(ctx.st, repo_id) {
        Some(r) => r,
        None => return "UNKNOWN",
    };
    let pull = match repo.pull(number) {
        Some(p) => p.clone(),
        None => return "UNKNOWN",
    };
    if pull.state != "open" || pull.merged {
        return "UNKNOWN";
    }
    let (Some(base_tip), Some(head_tip)) = (
        repo.refs.get(&format!("refs/heads/{}", pull.base_ref)).copied(),
        repo.refs.get(&format!("refs/heads/{}", pull.head_ref)).copied(),
    ) else {
        return "UNKNOWN";
    };
    let base_map = repo
        .objects
        .get(&base_tip)
        .and_then(|o| crate::gitcore::parse_commit(&o.data))
        .map(|c| crate::gitcore::flatten_tree(repo, &c.tree))
        .unwrap_or_default();
    let head_map = repo
        .objects
        .get(&head_tip)
        .and_then(|o| crate::gitcore::parse_commit(&o.data))
        .map(|c| crate::gitcore::flatten_tree(repo, &c.tree))
        .unwrap_or_default();
    let origin_map = match crate::gitcore::merge_base(repo, &base_tip, &head_tip) {
        Some(mb) => repo
            .objects
            .get(&mb)
            .and_then(|o| crate::gitcore::parse_commit(&o.data))
            .map(|c| crate::gitcore::flatten_tree(repo, &c.tree))
            .unwrap_or_default(),
        None => Default::default(),
    };
    match crate::gitcore::merge_maps(&origin_map, &base_map, &head_map) {
        Ok(_) => "MERGEABLE",
        Err(_) => "CONFLICTING",
    }
}

// --------------------------------------------------------------- mutations

fn input_of<'a>(args: &'a Args) -> Option<&'a Json> {
    args.0.get("input")
}

fn repo_ref_from_gid(ctx: &mut Ctx, gid_text: &str) -> Option<(String, String, i64)> {
    let (kind, key) = parse_gid(gid_text)?;
    if kind != "Repository" {
        return None;
    }
    let dbid: i64 = key.parse().ok()?;
    let repo = repo_by_id(ctx.st, dbid)?;
    Some((repo.owner.clone(), repo.name.clone(), repo.id))
}

fn issue_ref_from_gid(ctx: &mut Ctx, gid_text: &str) -> Option<(String, String, i64, i64)> {
    let (kind, key) = parse_gid(gid_text)?;
    if kind != "Issue" && kind != "PullRequest" {
        return None;
    }
    let mut parts = key.split(':');
    let dbid: i64 = parts.next()?.parse().ok()?;
    let number: i64 = parts.next()?.parse().ok()?;
    let repo = repo_by_id(ctx.st, dbid)?;
    Some((repo.owner.clone(), repo.name.clone(), repo.id, number))
}

fn payload(type_name: &'static str, fields: Vec<(&str, GVal)>) -> GVal {
    let mut all: Vec<(&str, GVal)> = vec![("clientMutationId", GVal::Null)];
    all.extend(fields);
    GVal::obj(type_name, all)
}

fn resolve_mutation(ctx: &mut Ctx, field: &str, args: &Args) -> Result<GVal, String> {
    let input = input_of(args).cloned().unwrap_or(Json::Null);
    let text = |key: &str| input.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
    match field {
        "createIssue" => {
            let repo_gid = text("repositoryId").ok_or("createIssue needs a repositoryId")?;
            let (owner, name, repo_id) =
                repo_ref_from_gid(ctx, &repo_gid).ok_or("repositoryId does not name a repository")?;
            let title = text("title").unwrap_or_default();
            let body = text("body").unwrap_or_default();
            match crate::rest::do_create_issue(ctx.st, &owner, &name, &title, &body, &[]) {
                Ok((number, _)) => Ok(payload(
                    "CreateIssuePayload",
                    vec![(
                        "issue",
                        GVal::Obj(GObj { type_name: "Issue", data: ObjData::Issue(repo_id, number) }),
                    )],
                )),
                Err(resp) => Err(error_message_of(resp)),
            }
        }
        "closeIssue" => {
            let issue_gid = text("issueId")
                .or_else(|| text("id"))
                .ok_or("closeIssue needs an issueId")?;
            let (owner, name, repo_id, number) =
                issue_ref_from_gid(ctx, &issue_gid).ok_or("id does not name an issue")?;
            let repo = ctx.st.repo_mut(&owner, &name).ok_or("repository vanished")?;
            let issue = repo.issue_mut(number).ok_or("issue vanished")?;
            issue.state = "closed".to_string();
            issue.updated = crate::store::now_unix();
            repo.touch();
            Ok(payload(
                "CloseIssuePayload",
                vec![(
                    "issue",
                    GVal::Obj(GObj { type_name: "Issue", data: ObjData::Issue(repo_id, number) }),
                )],
            ))
        }
        "createPullRequest" => {
            let repo_gid = text("repositoryId").ok_or("createPullRequest needs a repositoryId")?;
            let (owner, name, repo_id) =
                repo_ref_from_gid(ctx, &repo_gid).ok_or("repositoryId does not name a repository")?;
            let title = text("title").unwrap_or_default();
            let body = text("body").unwrap_or_default();
            let head = text("headRefName").unwrap_or_default();
            let base = text("baseRefName").unwrap_or_default();
            match crate::rest::do_create_pull(ctx.st, &owner, &name, &title, &body, &head, &base) {
                Ok((number, _)) => Ok(payload(
                    "CreatePullRequestPayload",
                    vec![(
                        "pullRequest",
                        GVal::Obj(GObj {
                            type_name: "PullRequest",
                            data: ObjData::Pull(repo_id, number),
                        }),
                    )],
                )),
                Err(resp) => Err(error_message_of(resp)),
            }
        }
        "mergePullRequest" => {
            let pr_gid = text("pullRequestId").ok_or("mergePullRequest needs a pullRequestId")?;
            let (owner, name, repo_id, number) =
                issue_ref_from_gid(ctx, &pr_gid).ok_or("pullRequestId does not name a pull request")?;
            let method = text("mergeMethod").unwrap_or_else(|| "MERGE".to_string()).to_lowercase();
            match crate::rest::do_merge_pull(ctx.st, &owner, &name, number, &method) {
                Ok(_) => Ok(payload(
                    "MergePullRequestPayload",
                    vec![(
                        "pullRequest",
                        GVal::Obj(GObj {
                            type_name: "PullRequest",
                            data: ObjData::Pull(repo_id, number),
                        }),
                    )],
                )),
                Err(resp) => Err(error_message_of(resp)),
            }
        }
        "addComment" => {
            let subject_gid = text("subjectId").ok_or("addComment needs a subjectId")?;
            let (owner, name, repo_id, number) =
                issue_ref_from_gid(ctx, &subject_gid).ok_or("subjectId does not name an issue")?;
            let body = text("body").unwrap_or_default();
            if body.is_empty() {
                return Err("addComment needs a body".to_string());
            }
            let cid = ctx.st.next_comment_id;
            ctx.st.next_comment_id += 1;
            let repo = ctx.st.repo_mut(&owner, &name).ok_or("repository vanished")?;
            let issue = repo.issue_mut(number).ok_or("issue vanished")?;
            issue.comments.push(crate::store::Comment {
                id: cid,
                body,
                author: crate::store::VIEWER_LOGIN.to_string(),
                created: crate::store::now_unix(),
            });
            repo.touch();
            let comment = GVal::Obj(GObj {
                type_name: "IssueComment",
                data: ObjData::Comment(repo_id, number, cid),
            });
            Ok(payload(
                "AddCommentPayload",
                vec![
                    (
                        "commentEdge",
                        GVal::obj(
                            "IssueCommentEdge",
                            vec![
                                ("node", comment.clone()),
                                ("cursor", GVal::Str(cursor_of(0))),
                            ],
                        ),
                    ),
                    ("comment", comment),
                    (
                        "subject",
                        GVal::Obj(GObj { type_name: "Issue", data: ObjData::Issue(repo_id, number) }),
                    ),
                ],
            ))
        }
        _ => Err(field_error("Mutation", field)),
    }
}

/// Pull the `message` out of a REST error response for a GraphQL error.
fn error_message_of(resp: Response) -> String {
    match Json::parse(&resp.body) {
        Ok(json) => json
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("the operation failed")
            .to_string(),
        Err(_) => "the operation failed".to_string(),
    }
}

// ------------------------------------------------------------ introspection

fn type_object(name: &str) -> GVal {
    let (kind, fields, enums): (&str, &[&str], &[&str]) = match name {
        "Issue" => (
            "OBJECT",
            &["id", "databaseId", "number", "title", "state", "body", "url", "author", "labels", "comments", "createdAt", "updatedAt", "closed", "repository"],
            &[],
        ),
        "PullRequest" => (
            "OBJECT",
            &["id", "databaseId", "number", "title", "body", "state", "merged", "mergeable", "headRefName", "baseRefName", "headRefOid", "baseRefOid", "mergedAt", "mergeCommit", "files", "author", "createdAt", "updatedAt", "url", "repository", "isDraft", "comments"],
            &[],
        ),
        "Repository" => (
            "OBJECT",
            &["id", "databaseId", "name", "nameWithOwner", "owner", "description", "isPrivate", "defaultBranchRef", "viewerPermission", "hasIssuesEnabled", "hasPullRequestsEnabled", "issue", "issues", "pullRequest", "pullRequests", "url", "createdAt", "updatedAt", "isEmpty"],
            &[],
        ),
        "User" => ("OBJECT", &["id", "login", "name", "email", "url"], &[]),
        "Label" => ("OBJECT", &["id", "name", "color", "description"], &[]),
        "IssueComment" => ("OBJECT", &["id", "databaseId", "body", "author", "createdAt", "updatedAt", "url"], &[]),
        "Ref" => ("OBJECT", &["name", "target"], &[]),
        "Commit" => ("OBJECT", &["oid", "message", "committedDate"], &[]),
        "PageInfo" => ("OBJECT", &["hasNextPage", "hasPreviousPage", "startCursor", "endCursor"], &[]),
        "Query" => ("OBJECT", &["viewer", "repository", "node", "search"], &[]),
        "Mutation" => (
            "OBJECT",
            &["createIssue", "closeIssue", "createPullRequest", "mergePullRequest", "addComment"],
            &[],
        ),
        "SearchType" => ("ENUM", &[], &["ISSUE", "REPOSITORY", "USER"]),
        "IssueState" => ("ENUM", &[], &["OPEN", "CLOSED"]),
        "PullRequestState" => ("ENUM", &[], &["OPEN", "CLOSED", "MERGED"]),
        "MergeableState" => ("ENUM", &[], &["MERGEABLE", "CONFLICTING", "UNKNOWN"]),
        "PullRequestMergeMethod" => ("ENUM", &[], &["MERGE", "SQUASH", "REBASE"]),
        "OrderDirection" => ("ENUM", &[], &["ASC", "DESC"]),
        _ => return GVal::Null,
    };
    let field_objs: Vec<GVal> = fields
        .iter()
        .map(|f| {
            GVal::obj(
                "Field",
                vec![
                    ("name", GVal::Str(f.to_string())),
                    ("description", GVal::Null),
                    ("args", GVal::List(vec![])),
                    ("type", GVal::obj("Type", vec![("name", GVal::Null)])),
                    ("isDeprecated", GVal::Bool(false)),
                    ("deprecationReason", GVal::Null),
                ],
            )
        })
        .collect();
    let enum_objs: Vec<GVal> = enums
        .iter()
        .map(|e| {
            GVal::obj(
                "EnumValue",
                vec![
                    ("name", GVal::Str(e.to_string())),
                    ("description", GVal::Null),
                    ("isDeprecated", GVal::Bool(false)),
                    ("deprecationReason", GVal::Null),
                ],
            )
        })
        .collect();
    GVal::obj(
        "Type",
        vec![
            ("name", GVal::Str(name.to_string())),
            ("kind", GVal::Str(kind.to_string())),
            ("description", GVal::Null),
            ("fields", GVal::List(field_objs)),
            ("enumValues", GVal::List(enum_objs)),
            ("possibleTypes", GVal::List(vec![])),
            ("inputFields", GVal::List(vec![])),
            ("interfaces", GVal::List(vec![])),
        ],
    )
}

// -------------------------------------------------------------------- HTTP

pub fn handle(req: &Request) -> Response {
    if req.method != "POST" {
        return Response::error(405, "Method Not Allowed");
    }
    if !req.authed() {
        return Response::error(401, "Requires authentication");
    }
    let parsed = match Json::parse(&req.body) {
        Ok(v) => v,
        Err(_) => {
            return Response::json(
                400,
                Json::obj(vec![(
                    "errors",
                    Json::Arr(vec![Json::obj(vec![("message", Json::Str("Problems parsing JSON".to_string()))])]),
                )]),
            );
        }
    };
    match parsed {
        Json::Arr(entries) => {
            let results: Vec<Json> = entries.iter().map(execute_entry).collect();
            Response::json(200, Json::Arr(results))
        }
        entry => Response::json(200, execute_entry(&entry)),
    }
}

fn execute_entry(entry: &Json) -> Json {
    let query_text = match entry.get("query").and_then(|q| q.as_str()) {
        Some(q) => q.to_string(),
        None => {
            return Json::obj(vec![(
                "errors",
                Json::Arr(vec![Json::obj(vec![("message", Json::Str("no query given".to_string()))])]),
            )]);
        }
    };
    let toks = match lex(&query_text) {
        Ok(t) => t,
        Err(e) => return gql_error(&format!("syntax error: {e}")),
    };
    let mut parser = Parser { toks, pos: 0 };
    let defs = match parser.document() {
        Ok(d) => d,
        Err(e) => return gql_error(&format!("syntax error: {e}")),
    };
    let mut fragments: HashMap<String, (String, Vec<Selection>)> = HashMap::new();
    let mut operation: Option<(String, Option<String>, Vec<(String, Value)>, Vec<Selection>)> = None;
    let wanted_name = entry.get("operationName").and_then(|v| v.as_str()).map(|s| s.to_string());
    for def in defs {
        match def {
            Def::Fragment { name, type_cond, selections } => {
                fragments.insert(name, (type_cond, selections));
            }
            Def::Operation { kind, name, var_defaults, selections } => {
                let take = match &wanted_name {
                    Some(w) => name.as_deref() == Some(w.as_str()),
                    None => operation.is_none(),
                };
                if take {
                    operation = Some((kind, name, var_defaults, selections));
                }
            }
        }
    }
    let (kind, _, var_defaults, selections) = match operation {
        Some(op) => op,
        None => return gql_error("no operation found"),
    };
    let mut vars: HashMap<String, Json> = HashMap::new();
    for (name, value) in &var_defaults {
        vars.insert(name.clone(), eval(value, &HashMap::new()));
    }
    if let Some(provided) = entry.get("variables") {
        if let Json::Obj(fields) = provided {
            for (k, v) in fields {
                vars.insert(k.clone(), v.clone());
            }
        }
    }
    let root_type: &'static str = if kind == "mutation" { "Mutation" } else { "Query" };
    let root = GObj { type_name: root_type, data: ObjData::Map(vec![]) };
    let mut guard = store().write().unwrap();
    let mut ctx = Ctx { st: &mut guard, vars, fragments, errors: Vec::new() };
    let mut out: Vec<(String, Json)> = Vec::new();
    project_into(&mut ctx, &root, &selections, &mut out);
    let mut response = vec![("data".to_string(), Json::Obj(out))];
    if !ctx.errors.is_empty() {
        let errors: Vec<Json> = ctx
            .errors
            .iter()
            .map(|e| Json::obj(vec![("message", Json::Str(e.clone()))]))
            .collect();
        response.push(("errors".to_string(), Json::Arr(errors)));
    }
    Json::Obj(response)
}

fn gql_error(message: &str) -> Json {
    Json::obj(vec![(
        "errors",
        Json::Arr(vec![Json::obj(vec![("message", Json::Str(message.to_string()))])]),
    )])
}
