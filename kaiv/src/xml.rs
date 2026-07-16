//! XML import/export (`--features xml`) — a thin adapter over the
//! value hub, zero dependencies: the well-formed-subset parser is
//! hand-rolled like the JSON one, keeping raw source slices so
//! mixed-content elements can embed verbatim as `std/enc/xml`.
//!
//! Mapping: the root element becomes the single top-level namespace;
//! attributes become `@name` members (auto-quoted kaiv keys), text
//! content beside attributes becomes `#text`, a text-only element is
//! a plain string, repeated same-name siblings group into an array
//! at the first occurrence's position, and an empty element is
//! `!null`. Element text is untyped — no number/boolean sniffing;
//! XML text has no types without a schema. Namespaces stay verbatim
//! (`soap:Body` is a literal key, `xmlns:*` are ordinary
//! attributes): no resolution, lossless round-trip instead.
//!
//! Mixed content (non-whitespace text interleaved with child
//! elements) has no tree form — the whole element embeds verbatim
//! as `std/enc/xml` and splices back on export. Fidelity is
//! semantic: comments, processing instructions, CDATA-ness, and
//! formatting do not survive; element text is trimmed of
//! leading/trailing whitespace; sibling order across *different*
//! names is not preserved (members are a map); a single occurrence
//! cannot be told from a one-element list. DOCTYPE/DTD input is
//! rejected (no custom entities); character references and the five
//! predefined entities decode. Export edges: keys must be valid XML
//! names, empty arrays vanish (no representation), arrays directly
//! inside arrays are an error, and null/empty-string converge on
//! `<a/>`.

use crate::error::PipelineError;
use crate::json::{self, node_to_val, Val};

const MAX_DEPTH: usize = 512;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import(input: &[u8]) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let (name, val) = parse_doc(text)?;
    json::import_val(&[(name, val)], false)
}

/// A complete XML document → (root element name, hub value).
pub(crate) fn parse_doc(text: &str) -> Result<(String, Val), PipelineError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = P { s: text, i: 0 };
    p.declaration()?;
    p.misc()?;
    if p.starts("<!") {
        return Err(err("DOCTYPE/DTD is not supported"));
    }
    let root = p.element(0)?;
    p.misc()?;
    if p.i != p.s.len() {
        return Err(err("trailing content after the root element"));
    }
    Ok((root.name.to_string(), elem_val(&root)?))
}

// ------------------------------------------------------------- XML parse

/// A parsed element. `raw` is the verbatim source slice (tags
/// included) so mixed content can embed without re-serialization.
struct Elem<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, String)>,
    kids: Vec<Elem<'a>>,
    text: String,
    mixed: bool,
    raw: &'a str,
}

struct P<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> P<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn starts(&self, t: &str) -> bool {
        self.rest().starts_with(t)
    }

    fn eat(&mut self, t: &str) -> bool {
        let hit = self.starts(t);
        if hit {
            self.i += t.len();
        }
        hit
    }

    fn expect(&mut self, t: &str) -> Result<(), PipelineError> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(err(format!("expected `{t}` at byte {}", self.i)))
        }
    }

    fn ws(&mut self) {
        while matches!(
            self.rest().as_bytes().first(),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            self.i += 1;
        }
    }

    /// Advance past `end`, returning the skipped-over content.
    fn take_until(&mut self, end: &str) -> Result<&'a str, PipelineError> {
        let n = self
            .rest()
            .find(end)
            .ok_or_else(|| err(format!("unterminated construct: missing `{end}`")))?;
        let out = &self.rest()[..n];
        self.i += n + end.len();
        Ok(out)
    }

    /// The optional `<?xml …?>` declaration: UTF-8 only, then dropped.
    /// `<?xml` must be followed by whitespace — anything else is an
    /// ordinary processing instruction (`<?xml-stylesheet …`).
    fn declaration(&mut self) -> Result<(), PipelineError> {
        let after = self.rest().strip_prefix("<?xml");
        if !matches!(
            after.and_then(|r| r.bytes().next()),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            return Ok(());
        }
        self.i += 5;
        let decl = self.take_until("?>")?;
        match decl_encoding(decl) {
            Some(enc) if !enc.eq_ignore_ascii_case("utf-8") => {
                Err(err(format!("unsupported encoding: {enc} (UTF-8 only)")))
            }
            _ => Ok(()),
        }
    }

    /// Prolog/epilog misc: whitespace, comments, PIs.
    fn misc(&mut self) -> Result<(), PipelineError> {
        loop {
            self.ws();
            if self.eat("<!--") {
                self.take_until("-->")?;
            } else if self.starts("<?") {
                self.i += 2;
                self.take_until("?>")?;
            } else {
                return Ok(());
            }
        }
    }

    /// An XML Name (liberal: ASCII per the spec shape, any non-ASCII
    /// accepted).
    fn name(&mut self) -> Result<&'a str, PipelineError> {
        let rest = self.rest();
        let mut end = 0;
        for (j, c) in rest.char_indices() {
            let ok = if j == 0 {
                c.is_ascii_alphabetic() || c == '_' || c == ':' || !c.is_ascii()
            } else {
                c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-' | '.') || !c.is_ascii()
            };
            if !ok {
                break;
            }
            end = j + c.len_utf8();
        }
        if end == 0 {
            return Err(err(format!("expected a name at byte {}", self.i)));
        }
        self.i += end;
        Ok(&rest[..end])
    }

    fn attr_value(&mut self) -> Result<String, PipelineError> {
        let q = match self.rest().as_bytes().first() {
            Some(b'"') => "\"",
            Some(b'\'') => "'",
            _ => return Err(err("attribute value must be quoted")),
        };
        self.i += 1;
        let raw = self.take_until(q)?;
        if raw.contains('<') {
            return Err(err("`<` in attribute value"));
        }
        decode_text(raw, true)
    }

    fn element(&mut self, depth: usize) -> Result<Elem<'a>, PipelineError> {
        if depth > MAX_DEPTH {
            return Err(err("XML nesting too deep"));
        }
        let start = self.i;
        self.expect("<")?;
        let name = self.name()?;
        let mut attrs: Vec<(&'a str, String)> = Vec::new();
        let empty = loop {
            self.ws();
            if self.eat("/>") {
                break true;
            }
            if self.eat(">") {
                break false;
            }
            let an = self.name()?;
            self.ws();
            self.expect("=")?;
            self.ws();
            let av = self.attr_value()?;
            if attrs.iter().any(|(k, _)| *k == an) {
                return Err(err(format!("duplicate attribute {an} on <{name}>")));
            }
            attrs.push((an, av));
        };
        let mut kids = Vec::new();
        let mut text = String::new();
        if !empty {
            loop {
                if self.rest().is_empty() {
                    return Err(err(format!("unclosed element <{name}>")));
                }
                if self.eat("</") {
                    let cname = self.name()?;
                    if cname != name {
                        return Err(err(format!("mismatched tags: <{name}> … </{cname}>")));
                    }
                    self.ws();
                    self.expect(">")?;
                    break;
                }
                if self.eat("<!--") {
                    self.take_until("-->")?;
                } else if self.eat("<![CDATA[") {
                    text.push_str(self.take_until("]]>")?);
                } else if self.starts("<!") {
                    return Err(err("DOCTYPE/DTD is not supported"));
                } else if self.eat("<?") {
                    self.take_until("?>")?;
                } else if self.starts("<") {
                    kids.push(self.element(depth + 1)?);
                } else {
                    let n = self.rest().find('<').unwrap_or(self.rest().len());
                    let raw = &self.rest()[..n];
                    self.i += n;
                    text.push_str(&decode_text(raw, false)?);
                }
            }
        }
        let mixed = !kids.is_empty() && !text.trim().is_empty();
        Ok(Elem {
            name,
            attrs,
            kids,
            text,
            mixed,
            raw: &self.s[start..self.i],
        })
    }
}

/// `encoding="…"` from an XML declaration body, if present.
fn decl_encoding(decl: &str) -> Option<&str> {
    let rest = decl[decl.find("encoding")? + "encoding".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let q = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let rest = &rest[1..];
    Some(&rest[..rest.find(q)?])
}

/// Character data / attribute value → decoded text: entities and
/// character references resolve, line ends normalize to `\n` (per
/// XML 1.0), and in attribute values literal whitespace becomes a
/// space (attribute-value normalization).
fn decode_text(raw: &str, attr: bool) -> Result<String, PipelineError> {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i..].chars().next().expect("in-bounds char boundary");
        match c {
            '&' => {
                let end = raw[i..]
                    .find(';')
                    .ok_or_else(|| err("unterminated entity reference"))?
                    + i;
                out.push(decode_entity(&raw[i + 1..end])?);
                i = end + 1;
            }
            '\r' => {
                if raw[i + 1..].starts_with('\n') {
                    i += 1;
                }
                out.push(if attr { ' ' } else { '\n' });
                i += 1;
            }
            '\n' | '\t' if attr => {
                out.push(' ');
                i += 1;
            }
            c => {
                // Raw content is bound by the same Char production as
                // character references: a document carrying a raw C0
                // control or a non-character is not well-formed XML.
                if !is_xml_char(c) {
                    return Err(err(format!(
                        "character U+{:04X} is not a legal XML character",
                        c as u32
                    )));
                }
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    Ok(out)
}

/// The five predefined entities plus character references; anything
/// else would need a DTD, which is out of scope.
fn decode_entity(ent: &str) -> Result<char, PipelineError> {
    Ok(match ent {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "apos" => '\'',
        "quot" => '"',
        _ => {
            let cp = if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()
            } else if let Some(dec) = ent.strip_prefix('#') {
                dec.parse::<u32>().ok()
            } else {
                return Err(err(format!(
                    "unsupported entity reference: &{ent}; (DTD entities are not supported)"
                )));
            };
            let c = cp
                .and_then(char::from_u32)
                .ok_or_else(|| err(format!("invalid character reference: &{ent};")))?;
            if !is_xml_char(c) {
                return Err(err(format!(
                    "character reference &{ent}; is not a legal XML character"
                )));
            }
            c
        }
    })
}

// ------------------------------------------------------------ Elem → Val

fn elem_val(e: &Elem) -> Result<Val, PipelineError> {
    if e.mixed {
        return Ok(Val::Typed {
            lib: "std/enc".to_string(),
            name: "xml".to_string(),
            text: json::b64url_encode(e.raw.as_bytes()),
        });
    }
    let text = e.text.trim();
    if e.attrs.is_empty() && e.kids.is_empty() {
        return Ok(if text.is_empty() {
            Val::Null
        } else {
            Val::Str(text.to_string())
        });
    }
    let mut members: Vec<(String, Val)> = e
        .attrs
        .iter()
        .map(|(k, v)| (format!("@{k}"), Val::Str(v.clone())))
        .collect();
    if !text.is_empty() {
        members.push(("#text".to_string(), Val::Str(text.to_string())));
    }
    // Kid names cannot collide with `@…`/`#text` members — XML names
    // never start with `@` or `#`.
    for kid in &e.kids {
        let v = elem_val(kid)?;
        match members.iter().position(|(k, _)| k == kid.name) {
            Some(pos) => {
                let slot = &mut members[pos].1;
                if let Val::Arr(items) = slot {
                    items.push(v);
                } else {
                    let first = std::mem::replace(slot, Val::Null);
                    *slot = Val::Arr(vec![first, v]);
                }
            }
            None => members.push((kid.name.to_string(), v)),
        }
    }
    Ok(Val::Obj(members))
}

// --------------------------------------------------------------- export

pub fn export(canonical: &str) -> Result<String, PipelineError> {
    let root = node_to_val(&json::tree(canonical)?)?;
    let Val::Obj(members) = root else {
        return Err(err("kaiv root is not a namespace"));
    };
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    // A single non-array top-level member is the root element (the
    // import shape); anything else wraps in <root>.
    if members.len() == 1 && !matches!(members[0].1, Val::Arr(_)) {
        emit(&mut out, &members[0].0, &members[0].1, 0)?;
    } else {
        emit(&mut out, "root", &Val::Obj(members), 0)?;
    }
    Ok(out)
}

/// One element named `name` holding `v`, at `depth`.
fn emit(out: &mut String, name: &str, v: &Val, depth: usize) -> Result<(), PipelineError> {
    if !name_ok(name) {
        return Err(err(format!("not a valid XML element name: {name}")));
    }
    let pad = "  ".repeat(depth);
    match v {
        Val::Typed {
            lib,
            name: tn,
            text,
        } if lib == "std/enc" && tn == "xml" => {
            let bytes =
                json::b64url_decode(text).ok_or_else(|| err("invalid base64url payload"))?;
            let xml =
                String::from_utf8(bytes).map_err(|_| err("std/enc/xml payload is not UTF-8"))?;
            let mut p = P { s: &xml, i: 0 };
            p.ws();
            let e = p.element(0)?;
            p.ws();
            if p.i != xml.len() {
                return Err(err("std/enc/xml payload is not a single element"));
            }
            if e.name != name {
                return Err(err(format!(
                    "std/enc/xml payload root <{}> does not match field {name}",
                    e.name
                )));
            }
            out.push_str(&format!("{pad}{}\n", xml.trim()));
        }
        Val::Obj(ms) => {
            let mut attrs = String::new();
            let mut text: Option<String> = None;
            let mut kids: Vec<(&String, &Val)> = Vec::new();
            for (k, mv) in ms {
                if let Some(an) = k.strip_prefix('@') {
                    if !name_ok(an) {
                        return Err(err(format!("not a valid XML attribute name: {an}")));
                    }
                    let t = scalar_text(mv)
                        .ok_or_else(|| err(format!("attribute @{an} holds a container")))?;
                    attrs.push_str(&format!(" {an}=\"{}\"", escape(&t, true)?));
                } else if k == "#text" {
                    let t = scalar_text(mv).ok_or_else(|| err("#text holds a container"))?;
                    text = Some(escape(&t, false)?);
                } else {
                    kids.push((k, mv));
                }
            }
            match (&text, kids.is_empty()) {
                (None, true) => out.push_str(&format!("{pad}<{name}{attrs}/>\n")),
                (Some(t), true) => out.push_str(&format!("{pad}<{name}{attrs}>{t}</{name}>\n")),
                _ => {
                    out.push_str(&format!("{pad}<{name}{attrs}>\n"));
                    if let Some(t) = &text {
                        out.push_str(&format!("{}{t}\n", "  ".repeat(depth + 1)));
                    }
                    for (k, mv) in kids {
                        emit_member(out, k, mv, depth + 1)?;
                    }
                    out.push_str(&format!("{pad}</{name}>\n"));
                }
            }
        }
        Val::Arr(_) => {
            return Err(err(format!(
                "array directly inside an array at {name} has no XML representation"
            )))
        }
        Val::Null => out.push_str(&format!("{pad}<{name}/>\n")),
        scalar => {
            let t = scalar_text(scalar).expect("containers handled above");
            if t.is_empty() {
                out.push_str(&format!("{pad}<{name}/>\n"));
            } else {
                out.push_str(&format!("{pad}<{name}>{}</{name}>\n", escape(&t, false)?));
            }
        }
    }
    Ok(())
}

/// One namespace member: an array fans out into repeated same-name
/// elements, everything else is a single element.
fn emit_member(out: &mut String, name: &str, v: &Val, depth: usize) -> Result<(), PipelineError> {
    if let Val::Arr(items) = v {
        for item in items {
            emit(out, name, item, depth)?;
        }
        Ok(())
    } else {
        emit(out, name, v, depth)
    }
}

/// Scalar → text; None for containers. Typed scalars (std/time,
/// std/num) emit their verbatim value; null degrades to the empty
/// string in attribute position.
fn scalar_text(v: &Val) -> Option<String> {
    match v {
        Val::Null => Some(String::new()),
        Val::Bool(b) => Some(b.to_string()),
        Val::Num(raw) => Some(raw.clone()),
        Val::Str(s) => Some(s.clone()),
        Val::Typed { text, .. } => Some(text.clone()),
        Val::Arr(_) | Val::Obj(_) => None,
    }
}

fn escape(s: &str, attr: bool) -> Result<String, PipelineError> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            '\n' if attr => out.push_str("&#10;"),
            '\t' if attr => out.push_str("&#9;"),
            '\r' => out.push_str("&#13;"),
            c if !is_xml_char(c) => {
                return Err(err(format!(
                    "character U+{:04X} has no XML representation",
                    c as u32
                )))
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// The XML 1.0 `Char` production: tab/LF/CR, and the Unicode ranges
/// excluding the remaining C0 controls, the surrogates, and the
/// non-characters U+FFFE/U+FFFF (C1 controls are permitted).
fn is_xml_char(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&c)
        || ('\u{E000}'..='\u{FFFD}').contains(&c)
        || ('\u{10000}'..='\u{10FFFF}').contains(&c)
}

/// Valid XML element/attribute name (same liberal shape the parser
/// accepts).
fn name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':' || !c.is_ascii() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-' | '.') || !c.is_ascii())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_control_reference_rejected_on_import() {
        assert!(import(b"<a>&#8;</a>").is_err());
        assert!(import(b"<a>&#10;</a>").is_ok());
        // Raw bytes are bound by the same Char production as refs.
        assert!(import(b"<a>\x08</a>").is_err());
        assert!(import(b"<a b=\"\x0c\"/>").is_err());
    }

    #[test]
    fn noncharacter_rejected_on_export() {
        assert!(export(".!kaiv 1\n!str'::a=\u{FFFF}\n").is_err());
    }

    fn roundtrip(src: &str) -> String {
        let authored = import(src.as_bytes()).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        export(&daiv).unwrap()
    }

    #[test]
    fn import_attributes_arrays_and_null() {
        let src = b"<server host=\"web01\" port=\"8080\">\n  <tags><tag>prod</tag><tag>eu</tag></tags>\n  <note/>\n</server>\n";
        let out = import(src).unwrap();
        // Attributes are @-members (quoted keys), element text is
        // untyped, repeated siblings group into an array, an empty
        // element is null.
        assert!(out.contains("/server::\"@host\"=web01\n"));
        assert!(out.contains("/server::\"@port\"=8080\n"));
        assert!(out.contains("/server/tags/@tag;=prod;eu\n"));
        assert!(out.contains("!null\n/server::note=\n"));
        assert!(!out.contains("&json"));
    }

    #[test]
    fn text_beside_attributes_is_hash_text() {
        let out = import(b"<greeting lang=\"en\">hello</greeting>").unwrap();
        assert!(out.contains("/greeting::\"@lang\"=en\n"));
        assert!(out.contains("/greeting::\"#text\"=hello\n"));
    }

    #[test]
    fn text_only_root_is_scalar() {
        let out = import(b"<greeting>hello</greeting>").unwrap();
        assert!(out.contains("greeting=hello\n"));
    }

    #[test]
    fn declaration_cdata_and_entities() {
        let src = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a id=\"1\">\n  <b><![CDATA[5 < 6 & 7]]></b>\n  <c>x &amp; y &#233;</c>\n</a>\n";
        let out = import(src).unwrap();
        assert!(out.contains("/a::\"@id\"=1\n"));
        assert!(out.contains("/a::b=5 < 6 & 7\n"));
        assert!(out.contains("/a::c=x & y \u{e9}\n"));
        let back = roundtrip(std::str::from_utf8(src).unwrap());
        assert!(back.contains("<b>5 &lt; 6 &amp; 7</b>"));
    }

    #[test]
    fn mixed_content_embeds_and_splices() {
        let src = "<doc>\n  <p>Hello <b>world</b>!</p>\n</doc>\n";
        let out = import(src.as_bytes()).unwrap();
        assert!(out.contains(".!types std/enc\n"));
        // The single typed member inlines: `&xml` + `/doc:=p={b64}`.
        let payload = out
            .lines()
            .skip_while(|l| *l != "&xml")
            .nth(1)
            .and_then(|l| l.strip_prefix("/doc:=p="))
            .unwrap();
        assert_eq!(
            json::b64url_decode(payload).unwrap(),
            b"<p>Hello <b>world</b>!</p>"
        );
        let back = roundtrip(src);
        assert!(back.contains("  <p>Hello <b>world</b>!</p>\n"));
    }

    #[test]
    fn semantic_roundtrip() {
        let src = "<config env=\"prod\">\n  <name>eu1</name>\n  <limits rps=\"500\" burst=\"900\"/>\n  <servers>\n    <server><host>a</host><port>1</port></server>\n    <server><host>b</host><port>2</port></server>\n  </servers>\n</config>\n";
        let back = roundtrip(src);
        assert_eq!(parse_doc(src).unwrap(), parse_doc(&back).unwrap());
    }

    #[test]
    fn namespaces_stay_verbatim() {
        let src = "<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\">\n  <soap:Body>\n    <m:GetPrice xmlns:m=\"https://example.com/prices\">\n      <m:Item currency=\"EUR\">Apples</m:Item>\n      <m:Item currency=\"EUR\">Pears</m:Item>\n    </m:GetPrice>\n  </soap:Body>\n</soap:Envelope>\n";
        let authored = import(src.as_bytes()).unwrap();
        assert!(authored.contains("\"@xmlns:soap\"=http://schemas.xmlsoap.org/soap/envelope/\n"));
        let back = roundtrip(src);
        assert!(back.contains("<m:Item currency=\"EUR\">Apples</m:Item>"));
        assert_eq!(parse_doc(src).unwrap(), parse_doc(&back).unwrap());
    }

    #[test]
    fn cross_format_json_to_xml() {
        let authored =
            crate::json::import(br#"{"config":{"name":"eu1","ports":[1,2],"on":true}}"#).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let xml = export(&daiv).unwrap();
        assert_eq!(
            xml,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n  <name>eu1</name>\n  <ports>1</ports>\n  <ports>2</ports>\n  <on>true</on>\n</config>\n"
        );
    }

    #[test]
    fn multiple_top_level_members_wrap_in_root() {
        let authored = crate::json::import(br#"{"a":"1","b":"2"}"#).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let xml = export(&daiv).unwrap();
        assert!(xml.contains("<root>\n  <a>1</a>\n  <b>2</b>\n</root>\n"));
    }

    #[test]
    fn multiline_attribute_survives_via_char_refs() {
        let src = "<a note=\"l1&#10;l2\"/>";
        let out = import(src.as_bytes()).unwrap();
        // The embedded newline forces the &json channel.
        assert!(out.contains("&json\n"));
        let back = roundtrip(src);
        assert!(back.contains("note=\"l1&#10;l2\""));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(import(b"<!DOCTYPE html><a/>").is_err());
        assert!(import(b"<a/><b/>").is_err());
        assert!(import(b"<a><b></a></b>").is_err());
        assert!(import(b"<a>&nbsp;</a>").is_err());
        assert!(import(b"<a x=\"1\" x=\"2\"/>").is_err());
        assert!(import(b"<?xml version=\"1.0\" encoding=\"UTF-16\"?><a/>").is_err());
        assert!(import(b"<a>").is_err());
    }
}
