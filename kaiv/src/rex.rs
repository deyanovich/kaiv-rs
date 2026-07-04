//! Minimal regex engine for the pinned kaiv pattern dialect (SPEC.md
//! § Formal Grammar, "Regex dialect"): literals, classes, `.`, anchors,
//! grouping, alternation, greedy bounded quantifiers, and the escapes
//! `\d` `\.` `\/` `\\` (any escaped punctuation is a literal).
//! Backtracking over the AST; no backreferences, no lookaround.

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class {
        neg: bool,
        ranges: Vec<(char, char)>,
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

#[derive(Debug, Clone)]
pub struct Regex {
    ast: Node,
}

impl Regex {
    pub fn new(pattern: &str) -> Option<Regex> {
        let cs: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        let ast = parse_alt(&cs, &mut i)?;
        if i != cs.len() {
            return None; // trailing garbage, e.g. unbalanced ')'
        }
        Some(Regex { ast })
    }

    /// Unanchored search (patterns anchor themselves with `^`/`$`).
    pub fn is_match(&self, text: &str) -> bool {
        let s: Vec<char> = text.chars().collect();
        (0..=s.len()).any(|start| m(&self.ast, &s, start, &|_j| true))
    }
}

fn parse_alt(cs: &[char], i: &mut usize) -> Option<Node> {
    let mut branches = vec![parse_seq(cs, i)?];
    while cs.get(*i) == Some(&'|') {
        *i += 1;
        branches.push(parse_seq(cs, i)?);
    }
    Some(if branches.len() == 1 {
        branches.pop().unwrap()
    } else {
        Node::Alt(branches)
    })
}

fn parse_seq(cs: &[char], i: &mut usize) -> Option<Node> {
    let mut nodes = Vec::new();
    while *i < cs.len() && cs[*i] != '|' && cs[*i] != ')' {
        let atom = parse_atom(cs, i)?;
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

fn parse_atom(cs: &[char], i: &mut usize) -> Option<Node> {
    let c = *cs.get(*i)?;
    *i += 1;
    match c {
        '(' => {
            let inner = parse_alt(cs, i)?;
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
                    ranges: vec![('0', '9')],
                }),
                // Escaped punctuation is literal. An escaped
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
            return Some(Node::Class { neg, ranges });
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
            e
        } else {
            *i += 1;
            c
        };
        if cs.get(*i) == Some(&'-') && cs.get(*i + 1).is_some_and(|&n| n != ']') {
            *i += 1;
            let mut hi = *cs.get(*i)?;
            if hi == '\\' {
                *i += 1;
                hi = *cs.get(*i)?;
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

/// Continuation-passing backtracking matcher.
fn m(node: &Node, s: &[char], i: usize, k: &dyn Fn(usize) -> bool) -> bool {
    match node {
        Node::Char(c) => i < s.len() && s[i] == *c && k(i + 1),
        Node::Any => i < s.len() && k(i + 1),
        Node::Class { neg, ranges } => i < s.len() && class_match(*neg, ranges, s[i]) && k(i + 1),
        Node::Start => i == 0 && k(i),
        Node::End => i == s.len() && k(i),
        Node::Seq(v) => m_seq(v, s, i, k),
        Node::Alt(v) => v.iter().any(|n| m(n, s, i, k)),
        Node::Rep { node, min, max } => m_rep(node, *min, *max, 0, s, i, k),
    }
}

fn m_seq(v: &[Node], s: &[char], i: usize, k: &dyn Fn(usize) -> bool) -> bool {
    match v.split_first() {
        None => k(i),
        Some((head, tail)) => m(head, s, i, &|j| m_seq(tail, s, j, k)),
    }
}

fn m_rep(
    node: &Node,
    min: u32,
    max: Option<u32>,
    done: u32,
    s: &[char],
    i: usize,
    k: &dyn Fn(usize) -> bool,
) -> bool {
    // Greedy: try one more repetition first (guarding zero-width loops),
    // then fall back to the continuation once the minimum is met.
    if max.is_none_or(|mx| done < mx)
        && m(node, s, i, &|j| {
            j > i && m_rep(node, min, max, done + 1, s, j, k)
        })
    {
        return true;
    }
    done >= min && k(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_alphanumerics_are_outside_the_dialect() {
        // `\1` is a backreference, `\w`/`\s`/`\b` are shorthand
        // classes/anchors -- all excluded; accepting them as literals
        // would silently change the pattern's meaning.
        for p in [r"^(a)\1$", r"\w+", r"a\s", r"\bword"] {
            assert!(Regex::new(p).is_none(), "{p} must be rejected");
        }
        // Escaped punctuation and \d stay in.
        assert!(Regex::new(r"a\.b\/c\\d").is_some());
        assert!(Regex::new(r"\d{2,4}").is_some());
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
