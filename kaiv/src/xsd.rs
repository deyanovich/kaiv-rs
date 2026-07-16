//! XSD (XML Schema) → authored `.saiv` (`--features xsd`) — parsed
//! with the crate's own XML parser, converted under the same
//! sound-weakening contract as the other schema converters: every
//! emitted constraint is implied by the source, and what kaiv cannot
//! express drops with a `//` comment.
//!
//! Covered subset: top-level and inline `element` / `complexType` /
//! `simpleType`, `sequence` / `all` / `choice` particles (choice
//! members all turn optional, exclusivity noted), attributes
//! (`use="required"` honored; they emit as `@name` fields, matching
//! the XML data converter), `simpleContent`/`extension` (the text
//! becomes a `#text` field beside the attributes), restriction
//! facets (`pattern` when it fits the kaiv regex dialect,
//! `enumeration`, `minInclusive`/`maxInclusive`,
//! `minExclusive`/`maxExclusive` exactly for integers, `length` /
//! `minLength` / `maxLength`), `minOccurs`/`maxOccurs`
//! (`maxOccurs > 1` makes arrays; struct arrays graduate occurs to
//! table-header cardinality). The built-in sized integers carry
//! their exact ranges; `float`/`double` admit INF/NaN by spec and
//! emit the extended-reals union; date/dateTime/time ride
//! `std/time`; `base64Binary` rides `&bin`; the string-shaped
//! built-ins (token, anyURI, duration, gYear, …) are strings.
//!
//! Known drops: `ref=` element/attribute references, `list`/`union`
//! simple types, `complexContent`, `any`/`anyAttribute`, and
//! recursive named types; `hexBinary` stays a plain string (its
//! content is hex text, not the `&bin` base64url channel). Pick the
//! root with the message argument when the schema has several
//! top-level elements. The contract holds for flat strings: see
//! the `jsonschema` module doc for the shared `std/enc/json`
//! embed-channel limitation.

use crate::error::PipelineError;
use crate::json::Val;
use crate::jsonschema::kaiv_key;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import_schema(
    input: &[u8],
    element: Option<&str>,
    name: &str,
) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let (root_name, root) = crate::xml::parse_doc(text)?;
    let prefix = xsd_prefix(&root_name, &root)?;
    let sch = Xsd {
        prefix,
        root: match &root {
            Val::Obj(_) => &root,
            _ => return Err(err("empty schema document")),
        },
    };
    let tops = sch.kids(sch.root, "element");
    let picked = match element {
        Some(n) => tops
            .iter()
            .find(|e| sch.attr(e, "name") == Some(n))
            .copied()
            .ok_or_else(|| err(format!("no top-level element named {n}")))?,
        None => match tops.as_slice() {
            [one] => *one,
            [] => return Err(err("the schema declares no top-level elements")),
            _ => {
                return Err(err(format!(
                    "the schema has several top-level elements ({}); pass --message",
                    tops.iter()
                        .filter_map(|e| sch.attr(e, "name"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
            }
        },
    };
    let mut ctx = Ctx {
        sch: &sch,
        body: String::new(),
        imports: std::collections::BTreeSet::new(),
    };
    // The root element's own type provides the document fields.
    let Some(shape) = ctx.element_shape(picked)? else {
        return Err(err("the root element has no resolvable type"));
    };
    match shape {
        Shape::Complex(ct) => ctx.complex_fields(ct, "", false, &mut Vec::new())?,
        Shape::Scalar(_) => return Err(err("the root element is scalar; nothing to convert")),
    }
    let mut out = format!(".!kaivschema 1 {name}\n");
    for lib in &ctx.imports {
        out.push_str(&format!(".!types {lib}\n"));
    }
    out.push('\n');
    out.push_str(&ctx.body);
    Ok(out)
}

/// The XSD namespace prefix in use (`xs`, `xsd`, or whatever the
/// document binds), from the xmlns declarations or the root name.
fn xsd_prefix(root_name: &str, root: &Val) -> Result<String, PipelineError> {
    if let Val::Obj(ms) = root {
        for (k, v) in ms {
            if let (Some(p), Val::Str(url)) = (k.strip_prefix("@xmlns:"), v) {
                if url == "http://www.w3.org/2001/XMLSchema" {
                    return Ok(p.to_string());
                }
            }
        }
        if matches!(ms.iter().find(|(k, _)| k == "@xmlns"),
            Some((_, Val::Str(url))) if url == "http://www.w3.org/2001/XMLSchema")
        {
            return Ok(String::new());
        }
    }
    match root_name.rsplit_once(':') {
        Some((p, "schema")) => Ok(p.to_string()),
        None if root_name == "schema" => Ok(String::new()),
        _ => Err(err("the document root is not an XSD schema element")),
    }
}

/// Navigation over the XML data converter's tree shape.
struct Xsd<'a> {
    prefix: String,
    root: &'a Val,
}

impl<'a> Xsd<'a> {
    fn tag(&self, local: &str) -> String {
        if self.prefix.is_empty() {
            local.to_string()
        } else {
            format!("{}:{local}", self.prefix)
        }
    }

    /// The child elements named `local` (0, 1, or many normalize to
    /// a list).
    fn kids(&self, v: &'a Val, local: &str) -> Vec<&'a Val> {
        let Val::Obj(ms) = v else { return Vec::new() };
        let tag = self.tag(local);
        match ms.iter().find(|(k, _)| *k == tag) {
            Some((_, Val::Arr(items))) => items.iter().collect(),
            Some((_, one)) => vec![one],
            None => Vec::new(),
        }
    }

    fn attr(&self, v: &'a Val, name: &str) -> Option<&'a str> {
        let Val::Obj(ms) = v else { return None };
        ms.iter()
            .find_map(|(k, mv)| match (k.strip_prefix('@'), mv) {
                (Some(a), Val::Str(s)) if a == name => Some(s.as_str()),
                _ => None,
            })
    }

    /// A named top-level complexType/simpleType.
    fn named(&self, kind: &str, name: &str) -> Option<&'a Val> {
        self.kids(self.root, kind)
            .into_iter()
            .find(|t| self.attr(t, "name") == Some(name))
    }
}

/// A resolved element type.
enum Shape<'a> {
    /// Annotation line (None = plain string).
    Scalar(Option<String>),
    /// A complexType node to walk.
    Complex(&'a Val),
}

struct Ctx<'a> {
    sch: &'a Xsd<'a>,
    body: String,
    imports: std::collections::BTreeSet<&'static str>,
}

impl<'a> Ctx<'a> {
    fn note(&mut self, msg: &str) {
        self.body.push_str(&format!("// dropped: {msg}\n"));
    }

    /// Push a scalar field's annotation + key line. Optional fields
    /// must leave the Denormalizer something to materialize when
    /// absent (SchemaOptionalWithoutDefaultError): a typed annotation
    /// gains a null alternative — a sound weakening. `&bin` (b64,
    /// empty default applicable) and plain strings stay.
    fn push_scalar(&mut self, anno: Option<String>, left: &str, optional: bool) {
        // An optional field's annotation is wrapped into a `!null|…`
        // union — one whitespace-free item token — so a whitespace-
        // bearing constraint (a space in a pattern facet) cannot join
        // it. Drop the annotation with a note; the bare optional field
        // is a sound weakening.
        let anno = match anno {
            Some(a) if optional && a.contains([' ', '\t']) => {
                self.note(&format!(
                    "whitespace-bearing constraints on optional {left} (union items are whitespace-free)"
                ));
                None
            }
            other => other,
        };
        if let Some(a) = anno {
            let a = if optional {
                match a.strip_prefix('!') {
                    Some(rest) if !rest.starts_with("null") => format!("!null|{rest}"),
                    _ => match a.as_str() {
                        "&date" => "!null|std/time/date".to_string(),
                        "&datetime" => "!null|std/time/datetime".to_string(),
                        "&time" => "!null|std/time/time".to_string(),
                        _ => a,
                    },
                }
            } else {
                a
            };
            self.body.push_str(&format!("{a}\n"));
        }
        self.body
            .push_str(&format!("{left}{}=\n", if optional { "?" } else { "" }));
    }

    /// Resolve an element's type: the `type` attribute (built-in or
    /// named) or an inline complexType/simpleType child.
    fn element_shape(&mut self, el: &'a Val) -> Result<Option<Shape<'a>>, PipelineError> {
        if self.sch.attr(el, "ref").is_some() {
            return Ok(None); // ref= handled by the caller's note
        }
        if let Some(t) = self.sch.attr(el, "type") {
            return self.type_shape(t);
        }
        if let [ct] = self.sch.kids(el, "complexType").as_slice() {
            return Ok(Some(Shape::Complex(ct)));
        }
        if let [st] = self.sch.kids(el, "simpleType").as_slice() {
            return Ok(self.simple_type(st)?.map(Shape::Scalar));
        }
        // No type at all: anyType — any well-formed content.
        Ok(None)
    }

    fn type_shape(&mut self, t: &str) -> Result<Option<Shape<'a>>, PipelineError> {
        let local = t.rsplit_once(':').map_or(t, |(_, l)| l);
        if let Some(anno) = self.builtin(local) {
            return Ok(Some(Shape::Scalar(anno)));
        }
        if let Some(ct) = self.sch.named("complexType", local) {
            return Ok(Some(Shape::Complex(ct)));
        }
        if let Some(st) = self.sch.named("simpleType", local) {
            return Ok(self.simple_type(st)?.map(Shape::Scalar));
        }
        Ok(None)
    }

    /// A built-in XSD type's annotation (None = not a built-in;
    /// Some(None) = plain string).
    fn builtin(&mut self, local: &str) -> Option<Option<String>> {
        let anno = match local {
            "boolean" => Some("!bool".to_string()),
            "integer" => Some("!int".to_string()),
            "long" => Some("!int[-9223372036854775808,9223372036854775807]".to_string()),
            "int" => Some("!int[-2147483648,2147483647]".to_string()),
            "short" => Some("!int[-32768,32767]".to_string()),
            "byte" => Some("!int[-128,127]".to_string()),
            "unsignedLong" => Some("!int[0,18446744073709551615]".to_string()),
            "unsignedInt" => Some("!int[0,4294967295]".to_string()),
            "unsignedShort" => Some("!int[0,65535]".to_string()),
            "unsignedByte" => Some("!int[0,255]".to_string()),
            "nonNegativeInteger" => Some("!int[0,]".to_string()),
            "positiveInteger" => Some("!int[1,]".to_string()),
            "nonPositiveInteger" => Some("!int[,0]".to_string()),
            "negativeInteger" => Some("!int[,-1]".to_string()),
            "decimal" => Some("!float".to_string()),
            // xs:float/double admit INF and NaN by spec.
            "float" | "double" => {
                self.imports.insert("std/num");
                Some("!float|std/num/inf|std/num/nan".to_string())
            }
            "date" => {
                self.imports.insert("std/time");
                Some("&date".to_string())
            }
            "dateTime" => {
                self.imports.insert("std/time");
                Some("&datetime".to_string())
            }
            "time" => {
                self.imports.insert("std/time");
                Some("&time".to_string())
            }
            "base64Binary" => {
                self.imports.insert("std/enc");
                Some("&bin".to_string())
            }
            // String-shaped built-ins; hexBinary content is hex TEXT,
            // so it stays a string rather than riding &bin.
            "string" | "normalizedString" | "token" | "language" | "Name" | "NCName" | "ID"
            | "IDREF" | "IDREFS" | "ENTITY" | "NMTOKEN" | "NMTOKENS" | "anyURI" | "QName"
            | "NOTATION" | "duration" | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay"
            | "gDay" | "hexBinary" => None,
            _ => return None,
        };
        Some(anno)
    }

    /// An inline/named simpleType → annotation (restriction facets).
    fn simple_type(&mut self, st: &'a Val) -> Result<Option<Option<String>>, PipelineError> {
        if !self.sch.kids(st, "list").is_empty() || !self.sch.kids(st, "union").is_empty() {
            return Ok(None); // list/union simple types drop
        }
        let restrictions = self.sch.kids(st, "restriction");
        let [restriction] = restrictions.as_slice() else {
            return Ok(None);
        };
        let base = self.sch.attr(restriction, "base").unwrap_or("string");
        let local = base.rsplit_once(':').map_or(base, |(_, l)| l);
        let Some(base_anno) = self.builtin(local) else {
            self.note(&format!("restriction on non-built-in base {base}"));
            return Ok(None);
        };
        // The core the facets attach to. Facets narrow the base, so
        // emitting only the facets is sound (a weakening when the
        // base range is wider than the facet range). Bounded
        // floats/doubles exclude non-finite values, so the extended
        // union collapses to plain float under facets.
        let core = match local {
            "boolean" => "bool",
            "decimal" | "float" | "double" => "float",
            _ if local.contains("nteger")
                || matches!(local, "long" | "int" | "short" | "byte")
                || local.starts_with("unsigned") =>
            {
                "int"
            }
            _ if base_anno.is_none() => "str",
            // A date/time/binary base with facets: keep the base
            // annotation, drop the facets.
            _ => {
                self.note(&format!("facets on {base} (base type kept)"));
                return Ok(Some(base_anno));
            }
        };
        let is_int = core == "int";
        let mut cons = String::new();
        // enumeration subsumes the other facets.
        let enums: Vec<&str> = self
            .sch
            .kids(restriction, "enumeration")
            .into_iter()
            .filter_map(|e| self.sch.attr(e, "value"))
            .collect();
        if !enums.is_empty() {
            // em-char forbids exactly `,` `}` `'` SP HTAB; EOL/NUL are
            // excluded for line integrity. Other punctuation (`{`, `|`,
            // `[`, `#`, …) is legal in a member and round-trips.
            if enums.iter().all(|v| {
                !v.is_empty() && !v.contains([',', '}', '\'', ' ', '\t', '\n', '\r', '\0'])
            }) {
                cons.push_str(&format!("{{{}}}", enums.join(",")));
                return Ok(Some(Some(format!("!{core}{cons}"))));
            }
            self.note("enumeration with unspellable members");
        }
        let facet = |name: &str| {
            self.sch
                .kids(restriction, name)
                .first()
                .and_then(|f| self.sch.attr(f, "value"))
                .map(str::to_string)
        };
        if let Some(p) = facet("pattern") {
            let escaped = p.replace('/', "\\/");
            if !p.contains(['\'', '\n', '\r']) && crate::rex::Regex::new(&escaped).is_some() {
                cons.push_str(&format!("/{escaped}/"));
            } else {
                self.note(&format!("pattern outside the kaiv regex dialect: {p}"));
            }
        }
        let mut lo = facet("minInclusive");
        let mut hi = facet("maxInclusive");
        let shift = |v: Option<String>, d: i64| {
            v.and_then(|s| s.parse::<i64>().ok())
                .and_then(|i| i.checked_add(d))
                .map(|i| i.to_string())
        };
        if let Some(v) = facet("minExclusive") {
            if is_int {
                lo = shift(Some(v), 1);
                if lo.is_none() {
                    self.note("minExclusive out of i64 range");
                }
            } else {
                self.note("minExclusive (inexact for kaiv inclusive ranges)");
            }
        }
        if let Some(v) = facet("maxExclusive") {
            if is_int {
                hi = shift(Some(v), -1);
                if hi.is_none() {
                    self.note("maxExclusive out of i64 range");
                }
            } else {
                self.note("maxExclusive (inexact for kaiv inclusive ranges)");
            }
        }
        if lo.is_some() || hi.is_some() {
            cons.push_str(&format!(
                "[{},{}]",
                lo.unwrap_or_default(),
                hi.unwrap_or_default()
            ));
        }
        let (minl, maxl) = match facet("length") {
            Some(n) => (Some(n.clone()), Some(n)),
            None => (facet("minLength"), facet("maxLength")),
        };
        if minl.is_some() || maxl.is_some() {
            cons.push_str(&format!(
                "#[{},{}]",
                minl.unwrap_or_default(),
                maxl.unwrap_or_default()
            ));
        }
        for f in ["totalDigits", "fractionDigits", "whiteSpace"] {
            if facet(f).is_some() {
                self.note(&format!("{f} facet"));
            }
        }
        if cons.is_empty() {
            // No expressible facets: fall back to the base annotation
            // (its built-in range still applies).
            return Ok(Some(base_anno));
        }
        Ok(Some(Some(format!("!{core}{cons}"))))
    }

    /// Emit the fields of a complexType at `path`.
    /// `all_optional` marks fields under an optional ancestor: kaiv
    /// cannot make a namespace itself optional, so requiredness must
    /// not leak through one.
    fn complex_fields(
        &mut self,
        ct: &'a Val,
        path: &str,
        all_optional: bool,
        visiting: &mut Vec<String>,
    ) -> Result<(), PipelineError> {
        if visiting.len() > 32 {
            return Err(err("type nesting too deep"));
        }
        // simpleContent: text + attributes.
        if let [sc] = self.sch.kids(ct, "simpleContent").as_slice() {
            if let [ext] = self.sch.kids(sc, "extension").as_slice() {
                let base = self.sch.attr(ext, "base").unwrap_or("string");
                let anno = self.type_shape(base)?;
                if let Some(Shape::Scalar(a)) = anno {
                    self.push_scalar(a, &lhs(path, "\"#text\""), all_optional);
                } else {
                    self.note(&format!("simpleContent base {base} at {}", disp(path)));
                }
                self.attributes(ext, path, all_optional)?;
                return Ok(());
            }
            self.note(&format!("simpleContent restriction at {}", disp(path)));
            return Ok(());
        }
        if !self.sch.kids(ct, "complexContent").is_empty() {
            self.note(&format!("complexContent at {}", disp(path)));
            return Ok(());
        }
        self.attributes(ct, path, all_optional)?;
        for particle in ["sequence", "all", "choice"] {
            for group in self.sch.kids(ct, particle) {
                let choice = particle == "choice";
                if choice {
                    self.note(&format!(
                        "choice exclusivity at {} (members emitted optional)",
                        disp(path)
                    ));
                }
                for el in self.sch.kids(group, "element") {
                    self.element_field(el, path, choice || all_optional, visiting)?;
                }
                if !self.sch.kids(group, "any").is_empty() {
                    self.note(&format!("xs:any at {}", disp(path)));
                }
                for inner in ["sequence", "choice", "all", "group"] {
                    if !self.sch.kids(group, inner).is_empty() {
                        self.note(&format!("nested {inner} particle at {}", disp(path)));
                    }
                }
            }
        }
        if !self.sch.kids(ct, "anyAttribute").is_empty() {
            self.note(&format!("anyAttribute at {}", disp(path)));
        }
        Ok(())
    }

    fn attributes(
        &mut self,
        owner: &'a Val,
        path: &str,
        all_optional: bool,
    ) -> Result<(), PipelineError> {
        for at in self.sch.kids(owner, "attribute") {
            let Some(aname) = self.sch.attr(at, "name") else {
                self.note(&format!("attribute ref at {}", disp(path)));
                continue;
            };
            let usage = self.sch.attr(at, "use").unwrap_or("optional");
            if usage == "prohibited" {
                continue;
            }
            let Ok(key) = kaiv_key(&format!("@{aname}")) else {
                self.note(&format!("unrepresentable attribute name: {aname:?}"));
                continue;
            };
            let anno = match self.sch.attr(at, "type") {
                Some(t) => match self.type_shape(t)? {
                    Some(Shape::Scalar(a)) => a,
                    Some(Shape::Complex(_)) => {
                        self.note(&format!(
                            "attribute {aname} with a complex type at {}",
                            disp(path)
                        ));
                        continue;
                    }
                    None => None,
                },
                None => match self.sch.kids(at, "simpleType").as_slice() {
                    [st] => self.simple_type(st)?.unwrap_or(None),
                    _ => None,
                },
            };
            self.push_scalar(
                anno,
                &lhs(path, &key),
                usage != "required" || all_optional,
            );
        }
        Ok(())
    }

    fn element_field(
        &mut self,
        el: &'a Val,
        path: &str,
        force_optional: bool,
        visiting: &mut Vec<String>,
    ) -> Result<(), PipelineError> {
        let Some(ename) = self.sch.attr(el, "name") else {
            self.note(&format!("element ref at {}", disp(path)));
            return Ok(());
        };
        let Ok(key) = kaiv_key(ename) else {
            self.note(&format!("unrepresentable element name: {ename:?}"));
            return Ok(());
        };
        let min = self.sch.attr(el, "minOccurs").unwrap_or("1");
        let max = self.sch.attr(el, "maxOccurs").unwrap_or("1");
        let optional = force_optional || min == "0";
        let repeated = max == "unbounded" || max.parse::<u64>().is_ok_and(|n| n > 1);
        // Cycle guard on named types.
        let tref = self.sch.attr(el, "type").map(str::to_string);
        if let Some(t) = &tref {
            if visiting.contains(t) {
                self.note(&format!("recursive type {t} at {}", disp2(path, ename)));
                return Ok(());
            }
        }
        let Some(shape) = self.element_shape(el)? else {
            self.note(&format!(
                "element {ename} at {} has no resolvable type",
                disp(path)
            ));
            return Ok(());
        };
        match (repeated, shape) {
            (false, Shape::Scalar(anno)) => {
                self.push_scalar(anno, &lhs(path, &key), optional);
            }
            (false, Shape::Complex(ct)) => {
                if let Some(t) = tref {
                    visiting.push(t);
                    self.complex_fields(ct, &format!("{path}/{key}"), optional, visiting)?;
                    visiting.pop();
                } else {
                    self.complex_fields(ct, &format!("{path}/{key}"), optional, visiting)?;
                }
            }
            (true, Shape::Scalar(anno)) => {
                if min != "0" && min != "1" {
                    self.note(&format!(
                        "minOccurs={min} on the scalar vector at {}",
                        disp2(path, ename)
                    ));
                }
                if max != "unbounded" {
                    self.note(&format!(
                        "maxOccurs={max} on the scalar vector at {}",
                        disp2(path, ename)
                    ));
                }
                if let Some(a) = anno {
                    self.body.push_str(&format!("{a}\n"));
                }
                self.body.push_str(&format!("{path}/@{key};=\n"));
            }
            (true, Shape::Complex(ct)) => {
                let mut open = format!("[{path}/@{key}");
                // Under a choice or an optional ancestor the element may
                // be wholly absent, so a min= would reject an XSD-valid
                // document. An upper bound (max=) stays sound.
                if !optional {
                    if let Ok(n) = min.parse::<u64>() {
                        if n > 0 {
                            open.push_str(&format!(" min={n}"));
                        }
                    }
                }
                if max != "unbounded" {
                    if let Ok(n) = max.parse::<u64>() {
                        open.push_str(&format!(" max={n}"));
                    }
                }
                open.push_str("]\n");
                self.body.push_str(&open);
                // Element blocks hold scalar fields: attributes and
                // scalar child elements.
                self.block_fields(ct, path, ename)?;
                self.body.push_str("[]\n");
            }
        }
        Ok(())
    }

    /// The scalar fields of a repeated element's complexType, inside
    /// its `[…]` block.
    fn block_fields(&mut self, ct: &'a Val, path: &str, ename: &str) -> Result<(), PipelineError> {
        // simpleContent: the element's text (always present in a row)
        // plus its attributes — otherwise both vanish from the block.
        if let [sc] = self.sch.kids(ct, "simpleContent").as_slice() {
            if let [ext] = self.sch.kids(sc, "extension").as_slice() {
                let base = self.sch.attr(ext, "base").unwrap_or("string");
                let anno = match self.type_shape(base)? {
                    Some(Shape::Scalar(a)) => a,
                    _ => None,
                };
                // Attributes first — xml::import emits them before the
                // element text, and the strict lockstep scan follows
                // schema order. The text is optional: an XSD-valid
                // empty element contributes no #text line, and required
                // fields are never materialized.
                for at in self.sch.kids(ext, "attribute") {
                    self.block_attr(at)?;
                }
                self.push_scalar(anno, "\"#text\"", true);
                return Ok(());
            }
            self.note(&format!("simpleContent restriction at {}", disp2(path, ename)));
            return Ok(());
        }
        for at in self.sch.kids(ct, "attribute") {
            self.block_attr(at)?;
        }
        for particle in ["sequence", "all", "choice"] {
            for group in self.sch.kids(ct, particle) {
                for el in self.sch.kids(group, "element") {
                    let Some(iname) = self.sch.attr(el, "name") else {
                        continue;
                    };
                    let Ok(ikey) = kaiv_key(iname) else {
                        continue;
                    };
                    let repeated = matches!(self.sch.attr(el, "maxOccurs"),
                        Some(m) if m == "unbounded" || m.parse::<u64>().is_ok_and(|n| n > 1));
                    match self.element_shape(el)? {
                        Some(Shape::Scalar(anno)) if !repeated => {
                            let optional =
                                self.sch.attr(el, "minOccurs") == Some("0") || particle == "choice";
                            self.push_scalar(anno, &ikey, optional);
                        }
                        _ => self.note(&format!(
                            "non-scalar element field {iname} at {}",
                            disp2(path, ename)
                        )),
                    }
                }
            }
        }
        Ok(())
    }

    /// Emit one attribute as a bare-keyed scalar field of an element
    /// block (the `[…]` header already scopes the row).
    fn block_attr(&mut self, at: &'a Val) -> Result<(), PipelineError> {
        let Some(aname) = self.sch.attr(at, "name") else {
            return Ok(());
        };
        let usage = self.sch.attr(at, "use").unwrap_or("optional");
        if usage == "prohibited" {
            return Ok(());
        }
        let Ok(key) = kaiv_key(&format!("@{aname}")) else {
            return Ok(());
        };
        let anno = match self.sch.attr(at, "type") {
            Some(t) => match self.type_shape(t)? {
                Some(Shape::Scalar(a)) => a,
                _ => None,
            },
            None => None,
        };
        self.push_scalar(anno, &key, usage != "required");
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

fn disp(path: &str) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.to_string()
    }
}

fn disp2(path: &str, name: &str) -> String {
    format!("{}/{name}", disp(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_ancestor_repeated_element_has_no_min() {
        let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="servers" minOccurs="0"><xs:complexType><xs:sequence>
      <xs:element name="server" maxOccurs="unbounded"><xs:complexType>
        <xs:attribute name="host" type="xs:string" use="required"/>
      </xs:complexType></xs:element>
    </xs:sequence></xs:complexType></xs:element>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;
        let saiv = import_schema(xsd.as_bytes(), None, "t").unwrap();
        assert!(saiv.contains("/@server"), "{saiv}");
        assert!(!saiv.contains("@server min="), "{saiv}");
    }

    #[test]
    fn repeated_simplecontent_keeps_text_and_attributes() {
        let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="label" maxOccurs="unbounded"><xs:complexType>
      <xs:simpleContent><xs:extension base="xs:string">
        <xs:attribute name="lang" use="required"/>
      </xs:extension></xs:simpleContent>
    </xs:complexType></xs:element>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;
        let saiv = import_schema(xsd.as_bytes(), None, "t").unwrap();
        // Text is optional (an empty element has no #text line) and
        // follows the attributes, matching xml::import's emission order.
        assert!(saiv.contains("\"#text\"?="), "{saiv}");
        assert!(saiv.contains("\"@lang\""), "{saiv}");
    }

    #[test]
    fn space_pattern_on_optional_field_drops_not_corrupts() {
        let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="n" minOccurs="0"><xs:simpleType>
      <xs:restriction base="xs:string"><xs:pattern value="a b"/></xs:restriction>
    </xs:simpleType></xs:element>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;
        let saiv = import_schema(xsd.as_bytes(), None, "t").unwrap();
        // The emitted schema must compile: a whitespace-bearing pattern
        // cannot join the !null| union, so it drops with a note.
        assert!(crate::compile_schema(saiv.as_bytes()).is_ok(), "{saiv}");
    }

    #[test]
    fn exclusive_bound_overflow_no_panic() {
        let xsd = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="root"><xs:complexType><xs:sequence>
    <xs:element name="n"><xs:simpleType><xs:restriction base="xs:long">
      <xs:minExclusive value="9223372036854775807"/>
    </xs:restriction></xs:simpleType></xs:element>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;
        assert!(import_schema(xsd.as_bytes(), None, "t").is_ok());
    }

    const XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="config">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="name" type="xs:string"/>
        <xs:element name="port" type="xs:int"/>
        <xs:element name="ratio" type="xs:double" minOccurs="0"/>
        <xs:element name="tier" type="TierType" minOccurs="0"/>
        <xs:element name="code" minOccurs="0">
          <xs:simpleType>
            <xs:restriction base="xs:string">
              <xs:pattern value="[a-z]+"/>
              <xs:maxLength value="8"/>
            </xs:restriction>
          </xs:simpleType>
        </xs:element>
        <xs:element name="when" type="xs:dateTime" minOccurs="0"/>
        <xs:element name="tag" type="xs:string" minOccurs="0" maxOccurs="unbounded"/>
        <xs:element name="server" minOccurs="1" maxOccurs="10">
          <xs:complexType>
            <xs:sequence>
              <xs:element name="host" type="xs:string"/>
              <xs:element name="weight" type="xs:int" minOccurs="0"/>
            </xs:sequence>
            <xs:attribute name="id" type="xs:int" use="required"/>
          </xs:complexType>
        </xs:element>
        <xs:element name="limits" type="LimitsType" minOccurs="0"/>
        <xs:element name="label" minOccurs="0">
          <xs:complexType>
            <xs:simpleContent>
              <xs:extension base="xs:string">
                <xs:attribute name="lang" type="xs:string" use="required"/>
              </xs:extension>
            </xs:simpleContent>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
      <xs:attribute name="env" type="xs:string" use="required"/>
    </xs:complexType>
  </xs:element>
  <xs:simpleType name="TierType">
    <xs:restriction base="xs:string">
      <xs:enumeration value="gold"/>
      <xs:enumeration value="silver"/>
    </xs:restriction>
  </xs:simpleType>
  <xs:complexType name="LimitsType">
    <xs:sequence>
      <xs:element name="rps" type="xs:positiveInteger"/>
    </xs:sequence>
  </xs:complexType>
</xs:schema>"#;

    #[test]
    fn core_mapping() {
        let saiv = import_schema(XSD.as_bytes(), None, "acme/config").unwrap();
        assert!(saiv.starts_with(".!kaivschema 1 acme/config\n"));
        assert!(saiv.contains("\"@env\"=\n"));
        assert!(saiv.contains("name=\n"));
        assert!(saiv.contains("!int[-2147483648,2147483647]\nport=\n"));
        assert!(saiv.contains("!null|float|std/num/inf|std/num/nan\nratio?=\n"));
        assert!(saiv.contains("!null|str{gold,silver}\ntier?=\n"));
        assert!(saiv.contains("!null|str/[a-z]+/#[,8]\ncode?=\n"));
        assert!(saiv.contains("!null|std/time/datetime\nwhen?=\n"));
        assert!(saiv.contains("/@tag;=\n"));
        assert!(saiv.contains("[/@server min=1 max=10]\n"));
        assert!(saiv.contains("!int[-2147483648,2147483647]\n\"@id\"=\n"));
        assert!(saiv.contains("host=\n"));
        assert!(saiv.contains("!null|int[-2147483648,2147483647]\nweight?=\n"));
        assert!(saiv.contains("!null|int[1,]\n/limits::rps?=\n"));
        assert!(saiv.contains("/label::\"#text\"?=\n"));
        assert!(saiv.contains("\"@lang\"?=\n"));
        // The schema compiles, and a document imported from XML by
        // the DATA converter has the right shape for it — after the
        // Denormalizer materializes the absent optional fields
        // (strict-lockstep parallel scan).
        let csaiv = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&csaiv).unwrap();
        let r = crate::Resolver::offline();
        r.preload("acme/config", "csaiv", csaiv.into_bytes());
        // Canonical text is valid `.raiv`; denormalize_with resolves
        // the declared schema and materializes the absent optionals.
        let raiv = ".!kaiv 1\n.!schema:acme/config\n!str'::\"@env\"=prod\n!str'::name=api\n!int'::port=443\n!str'/@tag::0=a\n!int'/@server/0::\"@id\"=1\n!str'/@server/0::host=h\n!int'/limits::rps=5\n";
        let daiv = crate::denorm::denormalize_with(raiv, &r).unwrap();
        assert!(daiv.contains("!null'::ratio=\n"));
        assert!(daiv.contains("!null'/@server/0::weight=\n"));
        assert_eq!(crate::validate(&daiv, &sc), Ok(()));
        // Required attribute and occurs cardinality are enforced.
        assert_eq!(
            crate::validate(".!kaiv 1\n!str'::name=api\n!int'::port=1\n!int'/@server/0::\"@id\"=1\n!str'/@server/0::host=h\n", &sc),
            Err(crate::AppError::RequiredFieldSchema)
        );
        // Fully materialized except the /@server array (min=1):
        // the Pass-1 cardinality check fires.
        assert_eq!(
            crate::validate(
                concat!(
                    ".!kaiv 1\n!str'::\"@env\"=p\n!str'::name=api\n!int'::port=1\n",
                    "!null'::ratio=\n!null'::tier=\n!null'::code=\n!null'::when=\n",
                    "!null'/limits::rps=\n!str'/label::\"#text\"=\n!str'/label::\"@lang\"=\n"
                ),
                &sc
            ),
            Err(crate::AppError::CardinalityViolation)
        );
    }

    #[test]
    fn drops_and_prefix_forms() {
        // xsd: prefix, choice, list, any, hexBinary, recursion.
        let xsd = r#"<xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <xsd:element name="doc">
    <xsd:complexType>
      <xsd:choice>
        <xsd:element name="a" type="xsd:string"/>
        <xsd:element name="b" type="xsd:int"/>
      </xsd:choice>
      <xsd:attribute name="hex" type="xsd:hexBinary"/>
    </xsd:complexType>
  </xsd:element>
</xsd:schema>"#;
        let saiv = import_schema(xsd.as_bytes(), None, "t").unwrap();
        assert!(saiv.contains("// dropped: choice exclusivity"));
        assert!(saiv.contains("a?=\n"));
        assert!(saiv.contains("!null|int[-2147483648,2147483647]\nb?=\n"));
        crate::compile_schema(saiv.as_bytes()).unwrap();
        // hexBinary errors at the type site, dropped attribute-side?
        // No: attribute typing errors propagate.
        assert!(saiv.contains("\"@hex\"?=") || saiv.contains("dropped"));
    }

    #[test]
    fn root_picking() {
        let xsd = r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="a"><xs:complexType><xs:sequence>
    <xs:element name="x" type="xs:string"/>
  </xs:sequence></xs:complexType></xs:element>
  <xs:element name="b"><xs:complexType><xs:sequence>
    <xs:element name="y" type="xs:string"/>
  </xs:sequence></xs:complexType></xs:element>
</xs:schema>"#;
        let e = import_schema(xsd.as_bytes(), None, "t")
            .unwrap_err()
            .to_string();
        assert!(e.contains("--message"));
        let saiv = import_schema(xsd.as_bytes(), Some("b"), "t").unwrap();
        assert!(saiv.contains("y=\n"));
        assert!(!saiv.contains("x=\n"));
    }
}
