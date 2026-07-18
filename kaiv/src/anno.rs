//! Parsing of type annotations and constraint expressions — the two
//! surface forms of the constraint grammar (SPEC.md § Formal Grammar):
//! the whitespace-free annotation position (`!int[1,65535]:s?prov`) and
//! the space-separated constraint-line position (`/re/ ..num [1,65535]`).

pub const CORE_TYPES: &[&str] = &["int", "float", "bool", "null", "b64", "text", "str", "map"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    /// `/regex/` — body verbatim (with `\/` passed through).
    Pattern(String),
    /// `[min,max]` — either endpoint may be empty (half-open).
    Range(Option<String>, Option<String>),
    /// `{a,b,c}`
    Enum(Vec<String>),
    /// `#[...]` or `#{...}`
    Length(Box<Constraint>),
    /// `..num`, `..lex`, `..lex[tag]`, `..time`, `..ver`
    Span(String),
}

/// One union alternative: a type reference plus the constraints that
/// textually follow it — authored inline narrowing (`!null|int[1,3600]`
/// narrows the `int` alternative) or a compiled `.csaiv` group
/// (`int(/^-?[0-9]+$/..num[1,3600])`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnionAlt {
    pub name: String,
    pub constraints: Vec<Constraint>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Annotation {
    /// `int`, `str`, `std/net/port`, `map`, …
    pub type_name: String,
    /// Union alternatives after the first (`!null|str` → `[str]`).
    pub union: Vec<UnionAlt>,
    /// `map<T>` value type.
    pub map_value: Option<String>,
    /// The head type's constraints (those preceding any `|`).
    pub constraints: Vec<Constraint>,
    /// Raw (uncanonicalized) unit expression after `:`.
    pub unit: Option<String>,
    /// Raw provenance list after `?`.
    pub provenance: Option<String>,
}

/// The delimiter set of the alternative-delimiter pattern form
/// `re{sep}body{sep}` (SPEC.md § Patterns).
pub(crate) const RE_SEPS: &[char] = &[':', ';', '%', '~', '@', '#'];

fn is_seg_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_unit_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '*' | '/' | '^' | '~' | '-')
}

/// Parse a whitespace-free annotation string beginning with `!`.
/// Returns None on any grammar violation.
pub fn parse_annotation(s: &str) -> Option<Annotation> {
    let s = s.strip_prefix('!')?;
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut a = Annotation::default();

    // Type reference. The first segment decides: a core type name is
    // terminal (a following `/` starts a pattern constraint); anything
    // else is a library path whose segments are consumed while they
    // look like path segments.
    let first = read_seg(&cs, &mut i)?;
    if first == "map" && cs.get(i) == Some(&'<') {
        i += 1;
        let v = read_seg(&cs, &mut i)?;
        if cs.get(i) != Some(&'>') {
            return None;
        }
        i += 1;
        a.type_name = "map".into();
        a.map_value = Some(v);
    } else if CORE_TYPES.contains(&first.as_str()) {
        a.type_name = first;
    } else {
        let mut path = first;
        while cs.get(i) == Some(&'/') && cs.get(i + 1).is_some_and(|c| is_seg_char(*c)) {
            i += 1;
            let seg = read_seg(&cs, &mut i)?;
            path.push('/');
            path.push_str(&seg);
        }
        a.type_name = path;
    }

    // Constraints, unit, provenance, union — fixed prefix order is not
    // enforced here beyond what the delimiters imply. A constraint
    // textually after a `|alt` narrows that alternative; before any
    // `|` it belongs to the head type.
    while i < cs.len() {
        match cs[i] {
            '[' => {
                let c = read_range(&cs, &mut i)?;
                push_constraint(&mut a, c);
            }
            '{' => {
                let c = read_enum(&cs, &mut i)?;
                push_constraint(&mut a, c);
            }
            '/' => {
                let c = read_pattern(&cs, &mut i)?;
                push_constraint(&mut a, c);
            }
            '#' => {
                i += 1;
                let inner = match cs.get(i) {
                    Some('[') => read_range(&cs, &mut i)?,
                    Some('{') => read_enum(&cs, &mut i)?,
                    _ => return None,
                };
                push_constraint(&mut a, Constraint::Length(Box::new(inner)));
            }
            '(' => {
                // A `.csaiv` lowered-constraint group: whitespace-free,
                // self-delimiting items until `)` (SPEC.md § Tagged
                // unions). Attaches to the current alternative.
                i += 1;
                loop {
                    match cs.get(i) {
                        Some(')') => {
                            i += 1;
                            break;
                        }
                        Some('/') => {
                            let c = read_pattern(&cs, &mut i)?;
                            push_constraint(&mut a, c);
                        }
                        Some('[') => {
                            let c = read_range(&cs, &mut i)?;
                            push_constraint(&mut a, c);
                        }
                        Some('{') => {
                            let c = read_enum(&cs, &mut i)?;
                            push_constraint(&mut a, c);
                        }
                        Some('#') => {
                            i += 1;
                            let inner = match cs.get(i) {
                                Some('[') => read_range(&cs, &mut i)?,
                                Some('{') => read_enum(&cs, &mut i)?,
                                _ => return None,
                            };
                            push_constraint(&mut a, Constraint::Length(Box::new(inner)));
                        }
                        Some('.') => {
                            // `..span` — a bracketed locale tag belongs
                            // only to `..lex`; after any other span a
                            // `[` starts the next (range) item.
                            if cs.get(i + 1) != Some(&'.') {
                                return None;
                            }
                            let start = i;
                            i += 2;
                            while i < cs.len() && cs[i].is_ascii_alphabetic() {
                                i += 1;
                            }
                            let base: String = cs[start..i].iter().collect();
                            if base == "..lex" && cs.get(i) == Some(&'[') {
                                while i < cs.len() && cs[i] != ']' {
                                    i += 1;
                                }
                                if i == cs.len() {
                                    return None;
                                }
                                i += 1;
                            }
                            push_constraint(
                                &mut a,
                                Constraint::Span(cs[start..i].iter().collect()),
                            );
                        }
                        _ => return None,
                    }
                }
            }
            ':' => {
                i += 1;
                let start = i;
                while i < cs.len() && is_unit_char(cs[i]) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let u: String = cs[start..i].iter().collect();
                // Grammar + membership check (§ Built-in units): an
                // expression that does not canonicalize is invalid.
                crate::unit::canonicalize(&u)?;
                a.unit = Some(u);
            }
            '?' => {
                // Provenance lists contain no whitespace — and no
                // `'`: that is the canonical machine-form delimiter
                // (`!int?src#dpid'path=value`), so stopping there
                // keeps a canonical data line from parsing as a
                // full-line annotation (it must classify as rule-5
                // content). Trailing re-literals may follow,
                // space-separated.
                i += 1;
                let start = i;
                while i < cs.len() && !matches!(cs[i], ' ' | '\t' | '\'') {
                    i += 1;
                }
                a.provenance = Some(cs[start..i].iter().collect());
            }
            ' ' | '\t' => {
                // Trailing `re{sep}body{sep}` pattern literals — the
                // alternative-delimiter authored form is always
                // whitespace-separated from the annotation and from
                // its neighbors (SPEC.md § Patterns). They narrow the
                // current alternative, like glued constraints.
                let mut any = false;
                while i < cs.len() {
                    while i < cs.len() && matches!(cs[i], ' ' | '\t') {
                        i += 1;
                    }
                    if i >= cs.len() {
                        break;
                    }
                    let c = read_re_literal(&cs, &mut i)?;
                    if i < cs.len() && !matches!(cs[i], ' ' | '\t') {
                        return None;
                    }
                    push_constraint(&mut a, c);
                    any = true;
                }
                if !any {
                    return None;
                }
            }
            '|' => {
                i += 1;
                let mut path = read_seg(&cs, &mut i)?;
                while cs.get(i) == Some(&'/') && cs.get(i + 1).is_some_and(|c| is_seg_char(*c)) {
                    i += 1;
                    let seg = read_seg(&cs, &mut i)?;
                    path.push('/');
                    path.push_str(&seg);
                }
                a.union.push(UnionAlt {
                    name: path,
                    constraints: Vec::new(),
                });
            }
            _ => return None,
        }
    }
    Some(a)
}

/// Attach a constraint to the current alternative: the last union
/// alternative if any, else the head type.
fn push_constraint(a: &mut Annotation, c: Constraint) {
    match a.union.last_mut() {
        Some(alt) => alt.constraints.push(c),
        None => a.constraints.push(c),
    }
}

fn read_seg(cs: &[char], i: &mut usize) -> Option<String> {
    let start = *i;
    while *i < cs.len() && is_seg_char(cs[*i]) {
        *i += 1;
    }
    if *i == start {
        return None;
    }
    Some(cs[start..*i].iter().collect())
}

fn read_range(cs: &[char], i: &mut usize) -> Option<Constraint> {
    debug_assert_eq!(cs[*i], '[');
    *i += 1;
    let start = *i;
    while *i < cs.len() && cs[*i] != ']' {
        *i += 1;
    }
    if *i == cs.len() {
        return None;
    }
    let body: String = cs[start..*i].iter().collect();
    *i += 1;
    let (lo, hi) = body.split_once(',')?;

    // ep-char forbids apostrophe, space, tab, and comma in an endpoint
    // (`]` cannot appear here); an empty endpoint is a valid half-open
    // bound. Reject otherwise so the compiled constraint re-lexes.
    let ep_ok = |s: &str| s.is_empty() || !s.contains(['\'', ' ', '\t', ',']);
    if !ep_ok(lo) || !ep_ok(hi) {
        return None;
    }

    // a range without a comma is invalid
    let opt = |s: &str| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };
    Some(Constraint::Range(opt(lo), opt(hi)))
}

fn read_enum(cs: &[char], i: &mut usize) -> Option<Constraint> {
    debug_assert_eq!(cs[*i], '{');
    *i += 1;
    let start = *i;
    while *i < cs.len() && cs[*i] != '}' {
        *i += 1;
    }
    if *i == cs.len() {
        return None;
    }
    let body: String = cs[start..*i].iter().collect();
    *i += 1;
    if body.is_empty() {
        return None;
    }
    let members: Vec<String> = body.split(',').map(str::to_string).collect();
    // em-char forbids apostrophe, space, and tab in a member (comma is
    // the separator, `}` cannot appear); reject so an apostrophe in an
    // enum can't break the first-tick split at validation time.
    if members.iter().any(|m| m.contains(['\'', ' ', '\t'])) {
        return None;
    }
    Some(Constraint::Enum(members))
}

/// `re{sep}body{sep}` — the alternative-delimiter authored pattern
/// form. There is **no escaping**: the body simply may not contain
/// the chosen separator (or `'`, the first-`'` split invariant). The
/// body lowers here to the canonical slash-delimited spelling —
/// every unescaped `/` becomes `\/` — so rendering and the regex
/// engine see one form only.
fn read_re_literal(cs: &[char], i: &mut usize) -> Option<Constraint> {
    if cs.get(*i) != Some(&'r') || cs.get(*i + 1) != Some(&'e') {
        return None;
    }
    let sep = *cs.get(*i + 2)?;
    if !RE_SEPS.contains(&sep) {
        return None;
    }
    *i += 3;
    let mut body = String::new();
    let mut esc = false;
    loop {
        let c = *cs.get(*i)?; // unterminated literal → None
        *i += 1;
        if c == sep {
            return Some(Constraint::Pattern(body));
        }
        if c == '\'' {
            return None;
        }
        if esc {
            body.push(c);
            esc = false;
        } else if c == '\\' {
            body.push('\\');
            esc = true;
        } else if c == '/' {
            body.push_str("\\/");
        } else {
            body.push(c);
        }
    }
}

fn read_pattern(cs: &[char], i: &mut usize) -> Option<Constraint> {
    debug_assert_eq!(cs[*i], '/');
    *i += 1;
    let mut body = String::new();
    while *i < cs.len() {
        match cs[*i] {
            // p-char excludes a literal `'` — it would break the
            // first-`'` metadata/namepath split (SPEC.md § Formal
            // Grammar); `\'` is likewise outside the escape dialect.
            '\'' => return None,
            '\\' => {
                // Backslash escapes the next char for delimiter-scanning
                // purposes; both bytes stay in the pattern body.
                body.push('\\');
                *i += 1;
                if *i < cs.len() {
                    if cs[*i] == '\'' {
                        return None;
                    }
                    body.push(cs[*i]);
                    *i += 1;
                }
            }
            '/' => {
                *i += 1;
                return Some(Constraint::Pattern(body));
            }
            c => {
                body.push(c);
                *i += 1;
            }
        }
    }
    None // unterminated pattern
}

/// One item of a space-separated constraint line (`.taiv` metadata
/// lines and `.csaiv` left sides).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A `!`-token — `!str`, `!a|b`, or a full annotation with inline
    /// constraints/unit (`!int[1,65535]`, `!float:km`), as base-type
    /// references in `.taiv` definitions and type items in `.csaiv`.
    Anno(Box<Annotation>),
    /// `&name` base reference (same-library named type).
    Named(String),
    Constraint(Constraint),
}

/// End of a `!…` annotation token. Whitespace ends the token only at
/// paren depth 0: a union alternative's lowered `(…)` group may carry
/// a pattern whose *body* contains spaces, parens, or brackets (e.g.
/// the std/time datetime pattern's `[Tt ]`), so inside a group the
/// self-delimiting items — `/pattern/`, `[range]`, `{enum}` — are
/// skipped as wholes.
fn anno_token_end(cs: &[char], start: usize) -> usize {
    let mut i = start;
    let mut depth = 0usize;
    while i < cs.len() {
        match cs[i] {
            ' ' | '\t' if depth == 0 => break,
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '/' if depth > 0 => {
                // Pattern item inside a group: consume to its
                // unescaped closing `/` (backslash-escape-aware).
                i += 1;
                let mut esc = false;
                while i < cs.len() {
                    let c = cs[i];
                    i += 1;
                    if esc {
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '/' {
                        break;
                    }
                }
            }
            '[' if depth > 0 => {
                while i < cs.len() && cs[i] != ']' {
                    i += 1;
                }
                i = (i + 1).min(cs.len());
            }
            '{' if depth > 0 => {
                while i < cs.len() && cs[i] != '}' {
                    i += 1;
                }
                i = (i + 1).min(cs.len());
            }
            _ => i += 1,
        }
    }
    i
}

/// Parse a space-separated constraint line. Patterns may contain
/// spaces, so this is a sequential scan, not a split.
pub fn parse_constraint_items(s: &str) -> Option<Vec<Item>> {
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut items = Vec::new();
    loop {
        while i < cs.len() && (cs[i] == ' ' || cs[i] == '\t') {
            i += 1;
        }
        if i >= cs.len() {
            break;
        }
        match cs[i] {
            '/' => items.push(Item::Constraint(read_pattern(&cs, &mut i)?)),
            '[' => items.push(Item::Constraint(read_range(&cs, &mut i)?)),
            '{' => items.push(Item::Constraint(read_enum(&cs, &mut i)?)),
            '#' => {
                i += 1;
                let inner = match cs.get(i) {
                    Some('[') => read_range(&cs, &mut i)?,
                    Some('{') => read_enum(&cs, &mut i)?,
                    _ => return None,
                };
                items.push(Item::Constraint(Constraint::Length(Box::new(inner))));
            }
            '.' => {
                // `..span` — consume to whitespace.
                if cs.get(i + 1) != Some(&'.') {
                    return None;
                }
                let start = i;
                while i < cs.len() && cs[i] != ' ' && cs[i] != '\t' {
                    i += 1;
                }
                items.push(Item::Constraint(Constraint::Span(
                    cs[start..i].iter().collect(),
                )));
            }
            '!' => {
                let start = i;
                i = anno_token_end(&cs, i);
                let tok: String = cs[start..i].iter().collect();
                let a = parse_annotation(&tok)?;
                if a.provenance.is_some() {
                    return None; // provenance is not a constraint item
                }
                items.push(Item::Anno(Box::new(a)));
            }
            '&' => {
                i += 1;
                let name = read_seg(&cs, &mut i)?;
                items.push(Item::Named(name));
            }
            'r' => {
                // `re{sep}body{sep}` — must stand alone between
                // whitespace boundaries.
                if i > 0 && !matches!(cs[i - 1], ' ' | '\t') {
                    return None;
                }
                let c = read_re_literal(&cs, &mut i)?;
                if i < cs.len() && !matches!(cs[i], ' ' | '\t') {
                    return None;
                }
                items.push(Item::Constraint(c));
            }
            _ => return None,
        }
    }
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_and_range_charset_is_enforced() {
        // Apostrophe / space in enum members and range endpoints are
        // outside em-char / ep-char and must be rejected.
        assert!(parse_constraint_items("{it,it's}").is_none());
        assert!(parse_constraint_items("{a b}").is_none());
        assert!(parse_constraint_items("[1, 2]").is_none());
        assert!(parse_constraint_items("[1,2,3]").is_none());
        // Clean forms still parse.
        assert!(parse_constraint_items("{a,b,c}").is_some());
        assert!(parse_constraint_items("[1,65535]").is_some());
        assert!(parse_constraint_items("[,5]").is_some());
        assert!(parse_constraint_items("[5,]").is_some());
    }

    #[test]
    fn unit_grammar_in_annotations() {
        assert!(parse_annotation("!float:km").is_some());
        assert!(parse_annotation("!float:m*s^-1").is_some());
        // Membership is context-dependent (imported custom units) and
        // checked separately via unit::members_ok — grammar accepts.
        assert!(parse_annotation("!float:kft").is_some());
        // Currency shape is grammar-level.
        assert!(parse_annotation("!float:~usd").is_none());
        assert!(parse_annotation("!float:~EURO").is_none());
    }

    #[test]
    fn union_alternative_constraints() {
        // Authored narrowing attaches to the alternative it follows.
        let a = parse_annotation("!null|int[1,3600]").unwrap();
        assert!(a.constraints.is_empty());
        assert_eq!(a.union.len(), 1);
        assert_eq!(a.union[0].name, "int");
        assert_eq!(a.union[0].constraints.len(), 1);

        // Compiled group form: whitespace-free, span not confused with
        // a following range.
        let c = parse_annotation("!null(/^$/)|int(/^-?[0-9]+$/..num[1,3600])").unwrap();
        assert_eq!(c.constraints.len(), 1); // head null: /^$/
        assert_eq!(c.union[0].constraints.len(), 3); // pattern, span, range
        assert!(matches!(
            &c.union[0].constraints[1],
            Constraint::Span(s) if s == "..num"
        ));
    }

    #[test]
    fn re_literals() {
        // Alternative delimiters, space-separated from the type;
        // slashes in the body lower to the canonical `\/` spelling.
        let a = parse_annotation("!str re~^https://[a-z.]+/.*~").unwrap();
        assert_eq!(
            a.constraints,
            vec![Constraint::Pattern(r"^https:\/\/[a-z.]+\/.*".into())]
        );
        // An already-escaped slash is not double-escaped.
        let b = parse_annotation(r"!str re~a\/b~").unwrap();
        assert_eq!(b.constraints, vec![Constraint::Pattern(r"a\/b".into())]);
        // All six separators.
        for sep in [':', ';', '%', '~', '@', '#'] {
            assert!(parse_annotation(&format!("!str re{sep}abc{sep}")).is_some());
        }
        // Union attachment: narrows the preceding alternative.
        let u = parse_annotation("!null|str re:^a$:").unwrap();
        assert!(u.constraints.is_empty());
        assert_eq!(
            u.union[0].constraints,
            vec![Constraint::Pattern("^a$".into())]
        );
        // Provenance then re-literal (data-file annotations).
        let p = parse_annotation("!str?src re:^a$:").unwrap();
        assert_eq!(p.provenance.as_deref(), Some("src"));
        assert_eq!(p.constraints.len(), 1);
    }

    #[test]
    fn re_literal_boundaries() {
        // Glued to the type: maximal-munch eats the name — invalid.
        assert!(parse_annotation("!strre:a:").is_none());
        // Glued to a following item — invalid.
        assert!(parse_constraint_items("re:a:{x,y}").is_none());
        assert!(parse_constraint_items("[1,2]re:a:").is_none());
        // Standalone and mixed items — valid; bodies may hold spaces.
        assert!(parse_constraint_items("re:a b: ..lex").is_some());
        assert!(parse_constraint_items("..num re%[0-9]+%").is_some());
        // Unterminated, body-contains-sep, apostrophe — invalid.
        assert!(parse_annotation("!str re:^a$").is_none());
        assert!(parse_annotation("!str re:it's:").is_none());
        // No escaping: a backslash does not protect the separator.
        let a = parse_annotation(r"!str re:a\:").unwrap();
        assert_eq!(a.constraints, vec![Constraint::Pattern("a\\".into())]);
    }

    #[test]
    fn pattern_rejects_apostrophe() {
        assert!(parse_annotation("!str/it's/").is_none());
        assert!(parse_annotation(r"!str/it\'s/").is_none());
        assert!(parse_constraint_items("/it's/ ..lex").is_none());
        assert!(parse_annotation("!str/its/").is_some());
    }
}
