//! Minimal regex engine for the pinned kaiv pattern dialect (SPEC.md
//! § Formal Grammar, "Regex dialect"): literals, classes, `.`, anchors,
//! grouping, alternation, greedy bounded quantifiers, and the escapes
//! `\d` `\.` `\/` `\\` (any escaped punctuation is a literal).
//!
//! Matching is an iterative Thompson/Pike NFA simulation over a compiled
//! instruction list: O(pattern) memory independent of input length, and
//! linear time in the input. This is the finite-state, constant-memory
//! execution model the spec pins — it cannot overflow the stack on long
//! inputs and cannot exhibit catastrophic backtracking (ReDoS). No
//! backreferences, no lookaround.

// Guards on untrusted pattern bodies (schema/type-library/imported
// JSON-Schema/XSD patterns): reject pathological input at compile time
// rather than let it exhaust memory. These bound only degenerate
// patterns — real schema patterns are orders of magnitude smaller.
const MAX_PATTERN_LEN: usize = 4096;
const MAX_PARSE_DEPTH: usize = 128;
const MAX_REPEAT: u32 = 1024;
const MAX_PROG: usize = 200_000;

use std::sync::Arc;

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class {
        neg: bool,
        // Shared, not owned: bounded repetition physically copies the
        // instruction, and an owned Vec would multiply a fat class's
        // payload by the repeat count (a ~4 KB hostile pattern could
        // reach gigabytes). An Arc clone is O(1).
        ranges: Arc<[(char, char)]>,
    },
    Start,
    End,
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Rep {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

/// One NFA instruction. `Split`/`Jmp` are epsilon transitions;
/// `Start`/`End` are zero-width assertions.
#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Any,
    Class {
        neg: bool,
        ranges: Arc<[(char, char)]>,
    },
    Start,
    End,
    Split(usize, usize),
    Jmp(usize),
    Match,
}

#[derive(Debug, Clone)]
pub struct Regex {
    prog: Vec<Inst>,
}

impl Regex {
    pub fn new(pattern: &str) -> Option<Regex> {
        if pattern.len() > MAX_PATTERN_LEN {
            return None;
        }
        let cs: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        let ast = parse_alt(&cs, &mut i, 0)?;
        if i != cs.len() {
            return None; // trailing garbage, e.g. unbalanced ')'
        }
        let mut prog = Vec::new();
        emit(&ast, &mut prog)?;
        prog.push(Inst::Match);
        Some(Regex { prog })
    }

    /// Unanchored search (patterns anchor themselves with `^`/`$`).
    pub fn is_match(&self, text: &str) -> bool {
        let s: Vec<char> = text.chars().collect();
        let prog = &self.prog;
        let len = s.len();
        let n = prog.len();

        // Pike NFA simulation: `clist` holds the epsilon-closed set of
        // consuming/accepting instruction pointers reachable at the
        // current position. A fresh start thread is seeded at every
        // position, which implements the unanchored search.
        let mut seen = vec![0u32; n];
        let mut gen = 0u32;
        let mut clist: Vec<usize> = Vec::new();
        let mut nlist: Vec<usize> = Vec::new();
        // One scratch stack reused across every epsilon closure: an
        // allocation per add_thread call would be O(input × program).
        let mut stack: Vec<usize> = Vec::new();

        gen += 1;
        add_thread(&mut clist, &mut stack, &mut seen, gen, prog, 0, 0, len);

        // `pos` indexes the input and drives the zero-width assertions,
        // so an index walk (including the final pos == len) is the right
        // shape here.
        #[allow(clippy::needless_range_loop)]
        for pos in 0..=len {
            if clist.iter().any(|&pc| matches!(prog[pc], Inst::Match)) {
                return true;
            }
            if pos == len {
                break;
            }
            let c = s[pos];
            nlist.clear();
            gen += 1;
            for &pc in &clist {
                let hit = match &prog[pc] {
                    Inst::Char(ch) => *ch == c,
                    Inst::Any => true,
                    Inst::Class { neg, ranges } => class_match(*neg, ranges, c),
                    _ => false,
                };
                if hit {
                    add_thread(&mut nlist, &mut stack, &mut seen, gen, prog, pc + 1, pos + 1, len);
                }
            }
            // Unanchored: a match may also begin at the next position.
            add_thread(&mut nlist, &mut stack, &mut seen, gen, prog, 0, pos + 1, len);
            std::mem::swap(&mut clist, &mut nlist);
        }
        false
    }
}

/// Follow epsilon transitions from `pc` (iteratively, so a long chain of
/// `Jmp`/`Split` cannot overflow the native stack), adding every
/// reachable consuming/accepting instruction to `list`. Zero-width
/// assertions are resolved against `pos`.
#[allow(clippy::too_many_arguments)]
fn add_thread(
    list: &mut Vec<usize>,
    stack: &mut Vec<usize>,
    seen: &mut [u32],
    gen: u32,
    prog: &[Inst],
    pc: usize,
    pos: usize,
    len: usize,
) {
    stack.clear();
    stack.push(pc);
    while let Some(pc) = stack.pop() {
        if seen[pc] == gen {
            continue;
        }
        seen[pc] = gen;
        match &prog[pc] {
            Inst::Jmp(x) => stack.push(*x),
            Inst::Split(a, b) => {
                stack.push(*b);
                stack.push(*a);
            }
            Inst::Start => {
                if pos == 0 {
                    stack.push(pc + 1);
                }
            }
            Inst::End => {
                if pos == len {
                    stack.push(pc + 1);
                }
            }
            _ => list.push(pc),
        }
    }
}

fn parse_alt(cs: &[char], i: &mut usize, depth: usize) -> Option<Node> {
    if depth > MAX_PARSE_DEPTH {
        return None;
    }
    let mut branches = vec![parse_seq(cs, i, depth)?];
    while cs.get(*i) == Some(&'|') {
        *i += 1;
        branches.push(parse_seq(cs, i, depth)?);
    }
    Some(if branches.len() == 1 {
        branches.pop().unwrap()
    } else {
        Node::Alt(branches)
    })
}

fn parse_seq(cs: &[char], i: &mut usize, depth: usize) -> Option<Node> {
    let mut nodes = Vec::new();
    while *i < cs.len() && cs[*i] != '|' && cs[*i] != ')' {
        let atom = parse_atom(cs, i, depth)?;
        nodes.push(parse_postfix(cs, i, atom)?);
    }
    Some(Node::Seq(nodes))
}

fn parse_postfix(cs: &[char], i: &mut usize, atom: Node) -> Option<Node> {
    let (min, max) = match cs.get(*i) {
        Some('*') => (0, None),
        Some('+') => (1, None),
        Some('?') => (0, Some(1)),
        Some('{') => {
            let close = cs[*i..].iter().position(|&c| c == '}')? + *i;
            let body: String = cs[*i + 1..close].iter().collect();
            *i = close; // advanced past '{'..'}' below
            let (lo, hi) = if let Some((a, b)) = body.split_once(',') {
                let lo: u32 = a.parse().ok()?;
                let hi = if b.is_empty() {
                    None
                } else {
                    Some(b.parse().ok()?)
                };
                (lo, hi)
            } else {
                let n: u32 = body.parse().ok()?;
                (n, Some(n))
            };
            // Transposed bounds (`a{3,1}`) are unsatisfiable — reject at
            // compile time, as mainstream engines do.
            if let Some(hi) = hi {
                if lo > hi {
                    return None;
                }
            }
            *i += 1;
            return Some(Node::Rep {
                node: Box::new(atom),
                min: lo,
                max: hi,
            });
        }
        _ => return Some(atom),
    };
    *i += 1;
    Some(Node::Rep {
        node: Box::new(atom),
        min,
        max,
    })
}

fn parse_atom(cs: &[char], i: &mut usize, depth: usize) -> Option<Node> {
    let c = *cs.get(*i)?;
    *i += 1;
    match c {
        '(' => {
            let inner = parse_alt(cs, i, depth + 1)?;
            if cs.get(*i) != Some(&')') {
                return None;
            }
            *i += 1;
            Some(inner)
        }
        '[' => parse_class(cs, i),
        '.' => Some(Node::Any),
        '^' => Some(Node::Start),
        '$' => Some(Node::End),
        '\\' => {
            let e = *cs.get(*i)?;
            *i += 1;
            match e {
                'd' => Some(Node::Class {
                    neg: false,
                    ranges: Arc::from(vec![('0', '9')]),
                }),
                // `\xHH` — exactly two hex digits naming an ASCII
                // character. The only letter escape besides `\d`:
                // unambiguous in every source dialect, and the way a
                // pattern carries the bytes the line grammar cannot
                // (the `'` delimiter is `\x27`).
                'x' => hex_escape(cs, i).map(Node::Char),
                // Escaped punctuation is literal. Any other escaped
                // alphanumeric is NOT: `\1` is a backreference and
                // `\w`/`\s`/`\b` are outside the pinned dialect --
                // treating them as literals would silently change the
                // pattern's meaning. Reject, so consumers (schema
                // lexer, importers) fail or drop loudly.
                e if e.is_ascii_alphanumeric() => None,
                _ => Some(Node::Char(e)), // escaped punctuation is literal
            }
        }
        '*' | '+' | '?' | '{' | ')' => None, // dangling metachar
        _ => Some(Node::Char(c)),
    }
}

/// `\xHH` after the `\x` has been consumed: exactly two hex digits
/// naming an ASCII character (00-7F). A missing or non-hex digit, or
/// a value past 7F, is outside the dialect. Consumes both digits.
fn hex_escape(cs: &[char], i: &mut usize) -> Option<char> {
    let h = cs.get(*i)?.to_digit(16)?;
    let l = cs.get(*i + 1)?.to_digit(16)?;
    let v = h * 16 + l;
    if v > 0x7F {
        return None;
    }
    *i += 2;
    char::from_u32(v)
}

fn parse_class(cs: &[char], i: &mut usize) -> Option<Node> {
    let neg = cs.get(*i) == Some(&'^');
    if neg {
        *i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let c = *cs.get(*i)?;
        if c == ']' && !first {
            *i += 1;
            return Some(Node::Class {
                neg,
                ranges: ranges.into(),
            });
        }
        first = false;
        let lo = if c == '\\' {
            *i += 1;
            let e = *cs.get(*i)?;
            *i += 1;
            if e == 'd' {
                ranges.push(('0', '9'));
                continue;
            }
            if e == 'x' {
                hex_escape(cs, i)?
            } else if e.is_ascii_alphanumeric() {
                // A shorthand class or backreference (`\w`,`\s`,`\1`)
                // is outside the dialect even inside a class — reject
                // rather than silently treat as the literal
                // letter/digit, matching parse_atom's escape policy.
                return None;
            } else {
                e
            }
        } else {
            *i += 1;
            c
        };
        if cs.get(*i) == Some(&'-') && cs.get(*i + 1).is_some_and(|&n| n != ']') {
            *i += 1;
            let mut hi = *cs.get(*i)?;
            if hi == '\\' {
                *i += 1;
                let e = *cs.get(*i)?;
                if e == 'x' {
                    *i += 1;
                    let h = hex_escape(cs, i)?;
                    ranges.push((lo, h));
                    continue;
                }
                hi = e;
                // `\d` and other shorthand escapes cannot be a range
                // endpoint.
                if hi.is_ascii_alphanumeric() {
                    return None;
                }
            }
            *i += 1;
            ranges.push((lo, hi));
        } else {
            ranges.push((lo, lo));
        }
    }
}

fn class_match(neg: bool, ranges: &[(char, char)], c: char) -> bool {
    let hit = ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi);
    hit != neg
}

/// Compile an AST node into the instruction list (Thompson
/// construction). Returns None if the program would exceed the size cap
/// — a bounded reject for a pathological pattern, never for a real one.
fn emit(node: &Node, prog: &mut Vec<Inst>) -> Option<()> {
    if prog.len() > MAX_PROG {
        return None;
    }
    match node {
        Node::Char(c) => prog.push(Inst::Char(*c)),
        Node::Any => prog.push(Inst::Any),
        Node::Class { neg, ranges } => prog.push(Inst::Class {
            neg: *neg,
            ranges: ranges.clone(),
        }),
        Node::Start => prog.push(Inst::Start),
        Node::End => prog.push(Inst::End),
        Node::Seq(v) => {
            for n in v {
                emit(n, prog)?;
            }
        }
        Node::Alt(v) => emit_alt(v, prog)?,
        Node::Rep { node, min, max } => emit_rep(node, *min, *max, prog)?,
    }
    Some(())
}

// Iterative on purpose: a flat alternation can carry thousands of
// branches within MAX_PATTERN_LEN, and per-branch recursion would be
// the one native-stack recursion not bounded by MAX_PARSE_DEPTH.
fn emit_alt(branches: &[Node], prog: &mut Vec<Inst>) -> Option<()> {
    let mut jmps = Vec::new();
    let last = branches.len() - 1;
    for (i, b) in branches.iter().enumerate() {
        if i < last {
            // split(body, next-branch); body ends with jmp(end).
            let sp = prog.len();
            prog.push(Inst::Split(0, 0));
            emit(b, prog)?;
            jmps.push(prog.len());
            prog.push(Inst::Jmp(0));
            prog[sp] = Inst::Split(sp + 1, prog.len());
        } else {
            emit(b, prog)?;
        }
    }
    let end = prog.len();
    for j in jmps {
        prog[j] = Inst::Jmp(end);
    }
    Some(())
}

fn emit_rep(node: &Node, min: u32, max: Option<u32>, prog: &mut Vec<Inst>) -> Option<()> {
    match max {
        None => {
            if min == 0 {
                // star: L1: split L2,L3 ; L2: <body> ; jmp L1 ; L3:
                let l1 = prog.len();
                let sp = prog.len();
                prog.push(Inst::Split(0, 0));
                let l2 = prog.len();
                emit(node, prog)?;
                prog.push(Inst::Jmp(l1));
                let l3 = prog.len();
                prog[sp] = Inst::Split(l2, l3);
            } else {
                if min > MAX_REPEAT {
                    return None;
                }
                for _ in 0..min {
                    emit(node, prog)?;
                }
                emit_rep(node, 0, None, prog)?;
            }
        }
        Some(mx) => {
            if min > mx || mx > MAX_REPEAT {
                return None;
            }
            for _ in 0..min {
                emit(node, prog)?;
            }
            // (mx - min) optional greedy copies, each able to short to a
            // common end.
            let mut splits = Vec::new();
            for _ in 0..(mx - min) {
                let sp = prog.len();
                prog.push(Inst::Split(0, 0));
                splits.push(sp);
                emit(node, prog)?;
            }
            let end = prog.len();
            for sp in splits {
                prog[sp] = Inst::Split(sp + 1, end);
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_escape_names_ascii_characters() {
        // \xHH is a literal ASCII character — the way a pattern
        // carries the line grammar's forbidden bytes (`'` = \x27).
        let re = Regex::new(r"^[a-z\x27]+$").unwrap();
        assert!(re.is_match("o'brien"));
        assert!(!re.is_match("o!brien"));
        let re = Regex::new(r"^\x41\x2fb$").unwrap(); // A, /, case-insensitive hex
        assert!(re.is_match("A/b"));
        // As a class range endpoint, both sides.
        let re = Regex::new(r"^[\x20-\x7e]+$").unwrap();
        assert!(re.is_match(" ~ printable"));
        assert!(!re.is_match("caf\u{e9}"));
        // Truncated, non-hex, or beyond ASCII: outside the dialect.
        for p in [r"\x", r"\x2", r"\xzz", r"\x80", r"[\x80]", r"[a\x]", r"^[\x20-\xff]$"] {
            assert!(Regex::new(p).is_none(), "{p} must be rejected");
        }
    }

    #[test]
    fn escaped_alphanumerics_are_outside_the_dialect() {
        // `\1` is a backreference, `\w`/`\s`/`\b` are shorthand
        // classes/anchors -- all excluded; accepting them as literals
        // would silently change the pattern's meaning.
        for p in [r"^(a)\1$", r"\w+", r"a\s", r"\bword"] {
            assert!(Regex::new(p).is_none(), "{p} must be rejected");
        }
        // The same holds inside a character class.
        for p in [r"[\w]+", r"[\s]", r"[a-\d]", r"[0-\d]"] {
            assert!(Regex::new(p).is_none(), "{p} must be rejected");
        }
        // Escaped punctuation and \d stay in.
        assert!(Regex::new(r"a\.b\/c\\d").is_some());
        assert!(Regex::new(r"\d{2,4}").is_some());
        assert!(Regex::new(r"[\d]").is_some());
        assert!(Regex::new(r"[\.\-]").is_some());
        assert!(Regex::new(r"[a-z\/]").is_some());
    }

    #[test]
    fn transposed_and_pathological_quantifiers_rejected() {
        assert!(Regex::new("a{3,1}").is_none());
        assert!(Regex::new(r"^\d{4,2}$").is_none());
        assert!(Regex::new("a{2,4}").is_some());
        assert!(Regex::new("a{3}").is_some());
        assert!(Regex::new("a{2,}").is_some());
        assert!(Regex::new("a{3,3}").is_some());
    }

    #[test]
    fn deeply_nested_or_huge_patterns_reject_without_overflow() {
        // Parser must not overflow the stack on hostile nesting.
        assert!(Regex::new(&"(".repeat(100_000)).is_none());
        // A modest number of groups still parses.
        assert!(Regex::new(&"(a)".repeat(60)).is_some());
    }

    #[test]
    fn fat_class_under_repetition_compiles_cheaply() {
        // A large class body times a nested bounded repeat: the repeat
        // physically copies the Class instruction, and the shared-Arc
        // ranges keep that O(copies), not O(copies × class size) — an
        // owned payload here measured in the gigabytes.
        let p = format!("([{}]{{1024}}){{100}}", "a".repeat(1000));
        assert!(Regex::new(&p).is_some());
    }

    #[test]
    fn flat_alternation_with_thousands_of_branches() {
        // emit_alt must not recurse per branch: a 2000-branch flat
        // alternation fits MAX_PATTERN_LEN and must compile and match
        // on any stack size.
        let p = vec!["a"; 2000].join("|");
        let re = Regex::new(&p).unwrap();
        assert!(re.is_match("a"));
        assert!(!re.is_match("b"));
    }

    #[test]
    fn long_inputs_match_without_overflow_or_blowup() {
        // A long value under `+` must MATCH (no false negative) and must
        // not overflow the stack — the whole point of the NFA rewrite.
        assert!(Regex::new("^[0-9]+$").unwrap().is_match(&"9".repeat(500_000)));
        assert!(Regex::new("^[A-Za-z0-9+/]*={0,2}$")
            .unwrap()
            .is_match(&"a".repeat(200)));
        // A non-matching long value returns false, also without overflow.
        let mut bad = "9".repeat(500_000);
        bad.push('a');
        assert!(!Regex::new("^[0-9]+$").unwrap().is_match(&bad));
    }

    #[test]
    fn no_catastrophic_backtracking() {
        // `^(a+)+$` on a non-matching run pins a backtracker for minutes;
        // the NFA returns immediately.
        let re = Regex::new("^(a+)+$").unwrap();
        let t = format!("{}X", "a".repeat(40));
        assert!(!re.is_match(&t));
        assert!(re.is_match("aaaa"));
    }

    fn ok(p: &str, t: &str) -> bool {
        Regex::new(p).unwrap().is_match(t)
    }

    #[test]
    fn core_patterns() {
        assert!(ok(r"^-?[0-9]+$", "8080"));
        assert!(ok(r"^-?[0-9]+$", "-3"));
        assert!(!ok(r"^-?[0-9]+$", "abc"));
        assert!(!ok(r"^-?[0-9]+$", "12a"));
        assert!(ok(r"^-?[0-9]*\.?[0-9]+([eE][+-]?[0-9]+)?$", "0.5"));
        assert!(ok(r"^-?[0-9]*\.?[0-9]+([eE][+-]?[0-9]+)?$", "1e-9"));
        assert!(!ok(r"^-?[0-9]*\.?[0-9]+([eE][+-]?[0-9]+)?$", "x"));
        assert!(ok(r"^$", ""));
        assert!(!ok(r"^$", "a"));
        assert!(ok(r"^[A-Za-z0-9+\/]*={0,2}$", "aGk="));
        assert!(ok(r"^(\d{1,3}\.){3}\d{1,3}$", "192.168.1.1"));
        assert!(!ok(r"^(\d{1,3}\.){3}\d{1,3}$", "192.168.1"));
        assert!(ok(r"[a-zA-Z0-9.-]+", "example.com"));
    }
}
