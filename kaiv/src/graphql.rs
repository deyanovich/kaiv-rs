//! GraphQL SDL → authored `.saiv` (`--features graphql`) — a sound
//! weakening like the other schema converters: every emitted
//! constraint is implied by the source, and what kaiv cannot express
//! drops with a `//` comment.
//!
//! The SDL parser covers type/input/interface definitions (all three
//! read as object shapes), enums, custom scalars, unions, schema
//! blocks, directives, descriptions, and field arguments; `extend`
//! is skipped. Pick the root type with the message argument, or let
//! the single object type in the document stand.
//!
//! Mapping: `Int` is 32-bit by spec (`!int[-2147483648,2147483647]`);
//! `Float` excludes non-finite values by spec (`!float`); `String`
//! and `ID` are unannotated strings; enums are closed
//! (`!str{…}`). Non-null fields (`T!`) are required; nullable fields
//! are optional and admit null (`!null|T` + `?=`). Object-typed
//! fields become namespaces (their requiredness rides the inner
//! fields), lists become vectors or element blocks. Recursive types
//! cannot unfold into a tree and drop with a note, as do fields
//! typed by custom scalars, unions, or nested lists — field
//! arguments are ignored (they shape queries, not data). The
//! contract holds for flat strings: see the `jsonschema` module
//! doc for the shared `std/enc/json` embed-channel limitation.

use crate::error::PipelineError;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import_schema(
    input: &[u8],
    type_name: Option<&str>,
    name: &str,
) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let doc = parse_sdl(text)?;
    let root = pick_type(&doc, type_name)?;
    let mut ctx = Ctx {
        doc: &doc,
        body: String::new(),
    };
    let GType::Object(fields) = &doc.types[root].shape else {
        return Err(err(format!(
            "{} is not an object type",
            doc.types[root].name
        )));
    };
    ctx.object_fields(fields, "", &mut vec![root])?;
    let mut out = format!(".!kaivschema 1 {name}\n\n");
    out.push_str(&ctx.body);
    Ok(out)
}

// ------------------------------------------------------------- the SDL

struct Doc {
    types: Vec<TypeDef>,
}

struct TypeDef {
    name: String,
    shape: GType,
}

enum GType {
    Object(Vec<Field>),
    Enum(Vec<String>),
    Scalar,
    Union,
}

struct Field {
    name: String,
    desc: Option<String>,
    ty: Tref,
}

/// A type reference: `T`, `T!`, `[T]`, `[T!]!`, … — list nullability
/// has no kaiv distinction (arrays are declared by their elements),
/// so only element/field non-null survives parsing.
enum Tref {
    Named(String, bool),
    List(Box<Tref>),
}

fn pick_type(doc: &Doc, name: Option<&str>) -> Result<usize, PipelineError> {
    match name {
        Some(n) => doc
            .types
            .iter()
            .position(|t| t.name == n)
            .ok_or_else(|| err(format!("no type named {n} in the document"))),
        None => {
            let objects: Vec<usize> = doc
                .types
                .iter()
                .enumerate()
                .filter(|(_, t)| matches!(t.shape, GType::Object(_)))
                .map(|(i, _)| i)
                .collect();
            match objects.as_slice() {
                [one] => Ok(*one),
                [] => Err(err("the document defines no object types")),
                _ => Err(err(format!(
                    "the document has several object types ({}); pass --message",
                    objects
                        .iter()
                        .map(|i| doc.types[*i].name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
            }
        }
    }
}

// ---------------------------------------------------------------- parse

#[derive(Clone, PartialEq, Debug)]
enum Tok {
    Name(String),
    Text(String),
    Punct(u8),
    Eof,
}

struct Lex<'a> {
    b: &'a [u8],
    i: usize,
}

impl Lex<'_> {
    fn ws(&mut self) {
        loop {
            // Commas are insignificant in GraphQL.
            while matches!(
                self.b.get(self.i),
                Some(b' ' | b'\t' | b'\r' | b'\n' | b',')
            ) {
                self.i += 1;
            }
            if self.b.get(self.i) == Some(&b'#') {
                while !matches!(self.b.get(self.i), None | Some(b'\n')) {
                    self.i += 1;
                }
            } else {
                return;
            }
        }
    }

    fn next(&mut self) -> Result<Tok, PipelineError> {
        self.ws();
        let Some(&c) = self.b.get(self.i) else {
            return Ok(Tok::Eof);
        };
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = self.i;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_alphanumeric() || *c == b'_') {
                self.i += 1;
            }
            return Ok(Tok::Name(
                String::from_utf8_lossy(&self.b[start..self.i]).into_owned(),
            ));
        }
        if c.is_ascii_digit()
            || (c == b'-' && matches!(self.b.get(self.i + 1), Some(d) if d.is_ascii_digit()))
        {
            // Numeric literals only appear in default values, which
            // are skipped structurally — lex loosely.
            let start = self.i;
            self.i += 1;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-'))
            {
                self.i += 1;
            }
            return Ok(Tok::Text(
                String::from_utf8_lossy(&self.b[start..self.i]).into_owned(),
            ));
        }
        if self.b[self.i..].starts_with(b"\"\"\"") {
            self.i += 3;
            let rest = &self.b[self.i..];
            let n = rest
                .windows(3)
                .position(|w| w == b"\"\"\"")
                .ok_or_else(|| err("unterminated block string"))?;
            let text = String::from_utf8_lossy(&rest[..n]).trim().to_string();
            self.i += n + 3;
            return Ok(Tok::Text(text));
        }
        if c == b'"' {
            self.i += 1;
            let start = self.i;
            loop {
                match self.b.get(self.i) {
                    None => return Err(err("unterminated string literal")),
                    Some(b'"') => break,
                    Some(b'\\') => self.i += 2,
                    Some(_) => self.i += 1,
                }
            }
            let text = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
            self.i += 1;
            return Ok(Tok::Text(text));
        }
        self.i += 1;
        Ok(Tok::Punct(c))
    }

    fn peek(&mut self) -> Result<Tok, PipelineError> {
        let save = self.i;
        let t = self.next()?;
        self.i = save;
        Ok(t)
    }

    fn expect(&mut self, p: u8) -> Result<(), PipelineError> {
        match self.next()? {
            Tok::Punct(c) if c == p => Ok(()),
            other => Err(err(format!(
                "expected `{}` in SDL, found {other:?}",
                p as char
            ))),
        }
    }

    fn name(&mut self) -> Result<String, PipelineError> {
        match self.next()? {
            Tok::Name(n) => Ok(n),
            other => Err(err(format!("expected a name in SDL, found {other:?}"))),
        }
    }

    /// Skip a balanced `(...)` / `{...}` / `[...]` group; the opener
    /// is already consumed.
    fn skip_group(&mut self, open: u8, close: u8) -> Result<(), PipelineError> {
        let mut depth = 1;
        while depth > 0 {
            match self.next()? {
                Tok::Eof => return Err(err("unterminated group in SDL")),
                Tok::Punct(c) if c == open => depth += 1,
                Tok::Punct(c) if c == close => depth -= 1,
                _ => {}
            }
        }
        Ok(())
    }

    /// Skip any `@directive(args)*` run.
    fn skip_directives(&mut self) -> Result<(), PipelineError> {
        while self.peek()? == Tok::Punct(b'@') {
            self.next()?;
            self.name()?;
            if self.peek()? == Tok::Punct(b'(') {
                self.next()?;
                self.skip_group(b'(', b')')?;
            }
        }
        Ok(())
    }

    fn type_ref(&mut self) -> Result<Tref, PipelineError> {
        let mut t = if self.peek()? == Tok::Punct(b'[') {
            self.next()?;
            let inner = self.type_ref()?;
            self.expect(b']')?;
            Tref::List(Box::new(inner))
        } else {
            Tref::Named(self.name()?, false)
        };
        if self.peek()? == Tok::Punct(b'!') {
            self.next()?;
            if let Tref::Named(n, _) = t {
                t = Tref::Named(n, true);
            }
        }
        Ok(t)
    }
}

fn parse_sdl(text: &str) -> Result<Doc, PipelineError> {
    let mut lex = Lex {
        b: text.as_bytes(),
        i: 0,
    };
    let mut doc = Doc { types: Vec::new() };
    loop {
        // A leading description on the definition.
        if matches!(lex.peek()?, Tok::Text(_)) {
            lex.next()?;
        }
        match lex.next()? {
            Tok::Eof => break,
            Tok::Name(kw) => match kw.as_str() {
                "type" | "input" | "interface" => {
                    let tname = lex.name()?;
                    if lex.peek()? == Tok::Name("implements".to_string()) {
                        lex.next()?;
                        lex.name()?;
                        while lex.peek()? == Tok::Punct(b'&') {
                            lex.next()?;
                            lex.name()?;
                        }
                    }
                    lex.skip_directives()?;
                    let mut fields = Vec::new();
                    if lex.peek()? == Tok::Punct(b'{') {
                        lex.next()?;
                        loop {
                            let desc = match lex.peek()? {
                                Tok::Text(t) => {
                                    lex.next()?;
                                    Some(t)
                                }
                                _ => None,
                            };
                            match lex.next()? {
                                Tok::Punct(b'}') => break,
                                Tok::Name(fname) => {
                                    if lex.peek()? == Tok::Punct(b'(') {
                                        lex.next()?;
                                        lex.skip_group(b'(', b')')?;
                                    }
                                    lex.expect(b':')?;
                                    let ty = lex.type_ref()?;
                                    // Input fields may default: `= value`.
                                    if lex.peek()? == Tok::Punct(b'=') {
                                        lex.next()?;
                                        match lex.peek()? {
                                            Tok::Punct(b'[') => {
                                                lex.next()?;
                                                lex.skip_group(b'[', b']')?;
                                            }
                                            Tok::Punct(b'{') => {
                                                lex.next()?;
                                                lex.skip_group(b'{', b'}')?;
                                            }
                                            _ => {
                                                lex.next()?;
                                            }
                                        }
                                    }
                                    lex.skip_directives()?;
                                    fields.push(Field {
                                        name: fname,
                                        desc,
                                        ty,
                                    });
                                }
                                other => {
                                    return Err(err(format!(
                                        "unexpected token in type {tname}: {other:?}"
                                    )))
                                }
                            }
                        }
                    }
                    doc.types.push(TypeDef {
                        name: tname,
                        shape: GType::Object(fields),
                    });
                }
                "enum" => {
                    let tname = lex.name()?;
                    lex.skip_directives()?;
                    lex.expect(b'{')?;
                    let mut values = Vec::new();
                    loop {
                        if matches!(lex.peek()?, Tok::Text(_)) {
                            lex.next()?;
                        }
                        match lex.next()? {
                            Tok::Punct(b'}') => break,
                            Tok::Name(v) => {
                                lex.skip_directives()?;
                                values.push(v);
                            }
                            other => {
                                return Err(err(format!(
                                    "unexpected token in enum {tname}: {other:?}"
                                )))
                            }
                        }
                    }
                    doc.types.push(TypeDef {
                        name: tname,
                        shape: GType::Enum(values),
                    });
                }
                "scalar" => {
                    let tname = lex.name()?;
                    lex.skip_directives()?;
                    doc.types.push(TypeDef {
                        name: tname,
                        shape: GType::Scalar,
                    });
                }
                "union" => {
                    let tname = lex.name()?;
                    lex.skip_directives()?;
                    if lex.peek()? == Tok::Punct(b'=') {
                        lex.next()?;
                        loop {
                            lex.name()?;
                            if lex.peek()? == Tok::Punct(b'|') {
                                lex.next()?;
                            } else {
                                break;
                            }
                        }
                    }
                    doc.types.push(TypeDef {
                        name: tname,
                        shape: GType::Union,
                    });
                }
                "schema" | "directive" | "extend" => {
                    // `extend` is followed by the extended definition's
                    // keyword (type/enum/...); consume it so the skip
                    // loop treats the extension body (not the keyword)
                    // as the thing to skip, rather than re-parsing it as
                    // a duplicate definition.
                    if kw == "extend" {
                        lex.next()?;
                    }
                    // Skip to the end of the definition (a balanced
                    // block if one opens, otherwise the next token
                    // run has no block to skip).
                    loop {
                        match lex.peek()? {
                            // A directive's argument list / a default
                            // object or list value is a balanced group,
                            // not the definition body — skip it whole so
                            // a nested `{` does not end the skip early.
                            Tok::Punct(b'(') => {
                                lex.next()?;
                                lex.skip_group(b'(', b')')?;
                            }
                            Tok::Punct(b'[') => {
                                lex.next()?;
                                lex.skip_group(b'[', b']')?;
                            }
                            Tok::Punct(b'{') => {
                                lex.next()?;
                                lex.skip_group(b'{', b'}')?;
                                break;
                            }
                            Tok::Eof => break,
                            Tok::Name(n)
                                if matches!(
                                    n.as_str(),
                                    "type"
                                        | "input"
                                        | "interface"
                                        | "enum"
                                        | "scalar"
                                        | "union"
                                        | "schema"
                                        | "directive"
                                        | "extend"
                                ) =>
                            {
                                break
                            }
                            _ => {
                                lex.next()?;
                            }
                        }
                    }
                }
                other => return Err(err(format!("unexpected `{other}` in SDL"))),
            },
            other => return Err(err(format!("unexpected token in SDL: {other:?}"))),
        }
    }
    Ok(doc)
}

// ----------------------------------------------------------------- emit

struct Ctx<'a> {
    doc: &'a Doc,
    body: String,
}

/// A resolved scalar-ish field type.
enum Core {
    Anno(Option<String>), // annotation line (None = plain string)
    Object(usize),        // object type index
    Drop(String),         // no kaiv spelling — reason
}

impl Ctx<'_> {
    fn note(&mut self, msg: &str) {
        self.body.push_str(&format!("// dropped: {msg}\n"));
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.doc.types.iter().position(|t| t.name == name)
    }

    /// Resolve a named type to its kaiv shape.
    fn core(&self, name: &str) -> Core {
        match name {
            "Int" => Core::Anno(Some("!int[-2147483648,2147483647]".to_string())),
            // GraphQL Float excludes non-finite values by spec.
            "Float" => Core::Anno(Some("!float".to_string())),
            "String" | "ID" => Core::Anno(None),
            "Boolean" => Core::Anno(Some("!bool".to_string())),
            other => match self.lookup(other) {
                Some(i) => match &self.doc.types[i].shape {
                    GType::Object(_) => Core::Object(i),
                    GType::Enum(vs) if !vs.is_empty() => {
                        Core::Anno(Some(format!("!str{{{}}}", vs.join(","))))
                    }
                    GType::Enum(_) => Core::Anno(None),
                    GType::Scalar => Core::Drop(format!("custom scalar {other}")),
                    GType::Union => Core::Drop(format!("union type {other}")),
                },
                None => Core::Drop(format!("undefined type {other}")),
            },
        }
    }

    fn object_fields(
        &mut self,
        fields: &[Field],
        path: &str,
        visiting: &mut Vec<usize>,
    ) -> Result<(), PipelineError> {
        if visiting.len() > 32 {
            return Err(err("type nesting too deep"));
        }
        for f in fields {
            let Ok(key) = crate::jsonschema::kaiv_key(&f.name) else {
                self.note(&format!("unrepresentable field name: {:?}", f.name));
                continue;
            };
            if let Some(d) = &f.desc {
                if !d.contains(['\n', '\r']) && !d.is_empty() {
                    self.body.push_str(&format!("// {d}\n"));
                }
            }
            match &f.ty {
                Tref::Named(tname, nonnull) => match self.core(tname) {
                    Core::Anno(anno) => {
                        let line = match (anno, nonnull) {
                            (Some(a), true) => Some(a),
                            (Some(a), false) => Some(format!("!null|{}", &a[1..])),
                            (None, true) => None,
                            (None, false) => Some("!null|str".to_string()),
                        };
                        if let Some(a) = line {
                            self.body.push_str(&format!("{a}\n"));
                        }
                        self.body.push_str(&format!(
                            "{}{}=\n",
                            lhs(path, &key),
                            if *nonnull { "" } else { "?" }
                        ));
                    }
                    Core::Object(i) => {
                        if visiting.contains(&i) {
                            self.note(&format!("recursive type at {}", disp2(path, &f.name)));
                            continue;
                        }
                        // A nullable object field cannot be expressed:
                        // materialization requires every declared field a
                        // line, so an inner required field cannot be made
                        // conditionally absent. Drop the subtree rather
                        // than emit unconditionally-required inner fields.
                        if !*nonnull {
                            self.note(&format!(
                                "nullable object field {} (kaiv cannot express conditional namespace presence)",
                                disp2(path, &f.name)
                            ));
                            continue;
                        }
                        let GType::Object(inner) = &self.doc.types[i].shape else {
                            unreachable!()
                        };
                        visiting.push(i);
                        self.object_fields(inner, &format!("{path}/{key}"), visiting)?;
                        visiting.pop();
                    }
                    Core::Drop(reason) => {
                        self.note(&format!("{reason} at {}", disp2(path, &f.name)))
                    }
                },
                Tref::List(el) => match el.as_ref() {
                    Tref::List(..) => {
                        self.note(&format!("nested list at {}", disp2(path, &f.name)))
                    }
                    Tref::Named(tname, el_nonnull) => match self.core(tname) {
                        Core::Anno(anno) => {
                            let line = match (anno, el_nonnull) {
                                (Some(a), true) => Some(a),
                                (Some(a), false) => Some(format!("!null|{}", &a[1..])),
                                (None, true) => None,
                                (None, false) => Some("!null|str".to_string()),
                            };
                            if let Some(a) = line {
                                self.body.push_str(&format!("{a}\n"));
                            }
                            self.body.push_str(&format!("{path}/@{key};=\n"));
                        }
                        Core::Object(i) => {
                            if visiting.contains(&i) {
                                self.note(&format!("recursive type at {}", disp2(path, &f.name)));
                                continue;
                            }
                            // A nullable list element is inexpressible: a
                            // kaiv namespace array cannot hold a null
                            // element, so its inner required fields cannot
                            // be conditionally absent. Drop the subtree.
                            if !*el_nonnull {
                                self.note(&format!(
                                    "nullable list element {} (kaiv arrays cannot hold null elements)",
                                    disp2(path, &f.name)
                                ));
                                continue;
                            }
                            let GType::Object(inner) = &self.doc.types[i].shape else {
                                unreachable!()
                            };
                            self.body.push_str(&format!("[{path}/@{key}]\n"));
                            for inf in inner {
                                let Ok(ikey) = crate::jsonschema::kaiv_key(&inf.name) else {
                                    self.note(&format!(
                                        "unrepresentable field name: {:?}",
                                        inf.name
                                    ));
                                    continue;
                                };
                                match &inf.ty {
                                    Tref::Named(itn, inn) => match self.core(itn) {
                                        Core::Anno(anno) => {
                                            let line = match (anno, inn) {
                                                (Some(a), true) => Some(a),
                                                (Some(a), false) => {
                                                    Some(format!("!null|{}", &a[1..]))
                                                }
                                                (None, true) => None,
                                                (None, false) => Some("!null|str".to_string()),
                                            };
                                            if let Some(a) = line {
                                                self.body.push_str(&format!("{a}\n"));
                                            }
                                            self.body.push_str(&format!(
                                                "{ikey}{}=\n",
                                                if *inn { "" } else { "?" }
                                            ));
                                        }
                                        _ => self.note(&format!(
                                            "non-scalar element field {} at {}",
                                            inf.name,
                                            disp2(path, &f.name)
                                        )),
                                    },
                                    Tref::List(..) => self.note(&format!(
                                        "non-scalar element field {} at {}",
                                        inf.name,
                                        disp2(path, &f.name)
                                    )),
                                }
                            }
                            self.body.push_str("[]\n");
                        }
                        Core::Drop(reason) => {
                            self.note(&format!("{reason} at {}", disp2(path, &f.name)))
                        }
                    },
                },
            }
        }
        Ok(())
    }
}

fn lhs(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}::{key}")
    }
}

fn disp2(path: &str, name: &str) -> String {
    if path.is_empty() {
        format!("root/{name}")
    } else {
        format!("{path}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extend_is_skipped_not_duplicated() {
        let sdl = "type Q { a: String } extend type Q { b: String } type Root { q: Q! }";
        assert!(import_schema(sdl.as_bytes(), Some("Root"), "t").is_ok());
    }

    #[test]
    fn nullable_object_field_dropped() {
        let sdl = "type Config { limits: Limits } type Limits { rps: Int! }";
        let saiv = import_schema(sdl.as_bytes(), Some("Config"), "t").unwrap();
        assert!(!saiv.contains("/limits::rps="), "{saiv}");
    }

    #[test]
    fn directive_with_object_default_parses() {
        let sdl =
            "input In { a: Int } directive @d(x: In = {a: 1}) on FIELD_DEFINITION type Root { n: Int! }";
        assert!(import_schema(sdl.as_bytes(), Some("Root"), "t").is_ok());
    }

    const SDL: &str = r#"
        "A service configuration."
        type Config {
            "The service name."
            name: String!
            port: Int!
            ratio: Float
            active: Boolean!
            tier: Tier
            tags: [String!]!
            servers(first: Int = 10): [Server!]
            limits: Limits!
            owner: Config
            custom: JSON
            thing: Thing
        }
        type Server { host: String! port: Int }
        type Limits { rps: Int! }
        enum Tier { GOLD SILVER }
        scalar JSON
        union Thing = Server | Limits
        schema { query: Config }
        directive @tag(name: String!) on FIELD_DEFINITION
    "#;

    #[test]
    fn core_mapping() {
        let saiv = import_schema(SDL.as_bytes(), Some("Config"), "acme/config").unwrap();
        assert!(saiv.starts_with(".!kaivschema 1 acme/config\n"));
        assert!(saiv.contains("// The service name.\nname=\n"));
        assert!(saiv.contains("!int[-2147483648,2147483647]\nport=\n"));
        assert!(saiv.contains("!null|float\nratio?=\n"));
        assert!(saiv.contains("!bool\nactive=\n"));
        assert!(saiv.contains("!null|str{GOLD,SILVER}\ntier?=\n"));
        assert!(saiv.contains("/@tags;=\n"));
        assert!(
            saiv.contains("[/@servers]\nhost=\n!null|int[-2147483648,2147483647]\nport?=\n[]\n")
        );
        assert!(saiv.contains("!int[-2147483648,2147483647]\n/limits::rps=\n"));
        assert!(saiv.contains("// dropped: recursive type at root/owner"));
        assert!(saiv.contains("// dropped: custom scalar JSON at root/custom"));
        assert!(saiv.contains("// dropped: union type Thing at root/thing"));
        // The schema compiles, and a conforming document validates.
        let csaiv = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&csaiv).unwrap();
        // Fully materialized: nullable-optional fields appear as
        // `!null'…=` lines (the Denormalizer would emit them; the
        // parallel scan is strict lockstep).
        let daiv = ".!kaiv 1\n!str'::name=api\n!int'::port=443\n!null'::ratio=\n!bool'::active=true\n!null'::tier=\n!str'/@tags::0=a\n!str'/@servers/0::host=h\n!null'/@servers/0::port=\n!int'/limits::rps=5\n";
        assert_eq!(crate::validate(daiv, &sc).map_err(|e| e.error), Ok(()));
        // Non-null fields are required; Int is 32-bit.
        assert_eq!(
            crate::validate(".!kaiv 1\n!str'::name=api\n", &sc).map_err(|e| e.error),
            Err(crate::AppError::RequiredFieldSchema)
        );
        let big = daiv.replace("port=443", "port=4430000000");
        assert_eq!(
            crate::validate(&big, &sc).map_err(|e| e.error),
            Err(crate::AppError::ConstraintViolation)
        );
    }

    #[test]
    fn type_picking_and_input_types() {
        // Several object types need an explicit pick.
        let e = import_schema(SDL.as_bytes(), None, "t")
            .unwrap_err()
            .to_string();
        assert!(e.contains("--message"));
        // A single input type stands alone; defaults are skipped.
        let sdl = r#"input Filter { q: String = "all" limit: Int = 10 }"#;
        let saiv = import_schema(sdl.as_bytes(), None, "t").unwrap();
        assert!(saiv.contains("!null|str\nq?=\n"));
        assert!(saiv.contains("!null|int[-2147483648,2147483647]\nlimit?=\n"));
        crate::compile_schema(saiv.as_bytes()).unwrap();
    }

    #[test]
    fn parse_rejects() {
        assert!(import_schema(b"type T { x }", None, "t").is_err());
        assert!(import_schema(b"frobnicate T {}", None, "t").is_err());
        assert!(import_schema(b"enum E { A }", None, "t")
            .unwrap_err()
            .to_string()
            .contains("no object types"));
    }
}
