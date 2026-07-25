//! Read-side access to canonical `.daiv` documents.
//!
//! [`builder`](crate::builder) is the write half — values in,
//! canonical lines out. This module is its mirror: parse a `.daiv`
//! stream once into an addressable [`Doc`], walk it with borrowed
//! [`View`]s (a view is the document scoped to a namepath prefix —
//! an array element, a sub-struct), and decode consumer types via
//! [`FromDaiv`]. A server answering typed requests reads
//! `MyParams::from_daiv(&doc.root().view("/params"))`; the builder
//! writes the reply. Neither half touches JSON.
//!
//! Scope: data documents only, and reading only — no schema
//! resolution, no validation (validate first with
//! [`validate`](crate::validate) when the input is untrusted; the
//! accessors here never interpret values beyond the line grammar).

use crate::error::PipelineError;
use crate::validator::{parse_daiv, DataLine};
use std::collections::BTreeMap;

/// A parsed canonical document: the data lines of one `.daiv`
/// stream, in document order, addressable by namepath.
pub struct Doc {
    lines: Vec<DataLine>,
}

impl Doc {
    /// Parse a canonical `.daiv` stream. Declarations and comments
    /// are skipped; every data line must be canonical
    /// (`!type'namepath=value`) or the parse fails.
    pub fn parse(daiv: &str) -> Result<Doc, PipelineError> {
        if let Err(e) = crate::lexer::expect_kind(daiv, "daiv") {
            return Err(PipelineError::Other(format!(
                "line {}: stream does not open with the .!daiv declaration",
                e.line
            )));
        }
        Ok(Doc {
            lines: parse_daiv(daiv)?,
        })
    }

    /// The scalar line at exactly `namepath`:
    /// `(type_name, value)`. Crate-internal — the serde
    /// deserializer's primitive.
    pub(crate) fn line_at(&self, namepath: &str) -> Option<(&str, &str)> {
        self.lines
            .iter()
            .find(|l| l.namepath == namepath)
            .map(|l| (l.type_name.as_str(), l.value.as_str()))
    }

    /// Every data line's namepath, in document order.
    pub(crate) fn namepaths(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|l| l.namepath.as_str())
    }

    /// The whole document as a [`View`] (empty namepath prefix).
    pub fn root(&self) -> View<'_> {
        View {
            doc: self,
            prefix: String::new(),
        }
    }
}

/// One typed value: the line's resolved type name (without `!`) and
/// its raw string value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Typed<'d> {
    /// Resolved type name — `str`, `null`, `std/time/datetime`, …
    pub type_name: &'d str,
    /// The raw value text after `=` (always a string — kaiv's one
    /// primitive).
    pub value: &'d str,
}

/// The document scoped to a namepath prefix. The root view's prefix
/// is empty; [`View::element`]s and [`View::view`]s extend it.
/// Lookup paths are relative: `::field` for a scalar at this level,
/// `/sub::field` below a struct, `@arr` for a nested scalar array.
#[derive(Clone)]
pub struct View<'d> {
    doc: &'d Doc,
    prefix: String,
}

impl<'d> View<'d> {
    fn abs(&self, path: &str) -> String {
        format!("{}{}", self.prefix, path)
    }

    /// The typed value at `path`, if the line exists.
    pub fn typed(&self, path: &str) -> Option<Typed<'d>> {
        let want = self.abs(path);
        self.doc.lines.iter().find(|l| l.namepath == want).map(|l| Typed {
            type_name: &l.type_name,
            value: &l.value,
        })
    }

    /// The raw value at `path`. A `!null` line yields `None` — use
    /// [`typed`](Self::typed) to distinguish null from absent.
    pub fn value(&self, path: &str) -> Option<&'d str> {
        match self.typed(path) {
            Some(t) if t.type_name != "null" => Some(t.value),
            _ => None,
        }
    }

    /// Is the line at `path` present and `!null`-typed?
    pub fn is_null(&self, path: &str) -> bool {
        self.typed(path).is_some_and(|t| t.type_name == "null")
    }

    /// The required value at `path` — [`value`](Self::value) with a
    /// namepath-bearing error for decoder use.
    pub fn required(&self, path: &str) -> Result<&'d str, PipelineError> {
        self.value(path).ok_or_else(|| {
            PipelineError::Other(format!("missing required field {}", self.abs(path)))
        })
    }

    /// The elements of the scalar array at `arr` (`{arr}::N` lines),
    /// in index order.
    pub fn scalars(&self, arr: &str) -> Vec<&'d str> {
        let prefix = format!("{}::", self.abs(arr));
        let mut by_idx: BTreeMap<usize, &str> = BTreeMap::new();
        for l in &self.doc.lines {
            if let Some(i) = l
                .namepath
                .strip_prefix(prefix.as_str())
                .and_then(|i| i.parse::<usize>().ok())
            {
                by_idx.entry(i).or_insert(l.value.as_str());
            }
        }
        by_idx.into_values().collect()
    }

    /// The element views of the namespace array at `arr`
    /// (`{arr}/N::…` lines), in index order.
    pub fn elements(&self, arr: &str) -> Vec<View<'d>> {
        let prefix = format!("{}/", self.abs(arr));
        let mut idxs: Vec<usize> = Vec::new();
        for l in &self.doc.lines {
            if let Some(rest) = l.namepath.strip_prefix(prefix.as_str()) {
                let digits: &str = rest.split(&['/', ':'][..]).next().unwrap_or("");
                if let Ok(i) = digits.parse::<usize>() {
                    if !idxs.contains(&i) {
                        idxs.push(i);
                    }
                }
            }
        }
        idxs.sort_unstable();
        idxs.into_iter()
            .map(|i| View {
                doc: self.doc,
                prefix: format!("{prefix}{i}"),
            })
            .collect()
    }

    /// The multi-line text at `path`: a `!text`-typed line's value
    /// with the `|:|` separators resolved to newlines; a plain
    /// `str`-typed line as-is (the str→text coercion's read side).
    /// `None` for null, absent, or any other type.
    pub fn text(&self, path: &str) -> Option<String> {
        let t = self.typed(path)?;
        match t.type_name {
            "text" => Some(t.value.replace("|:|", "\n")),
            "str" => Some(t.value.to_string()),
            _ => None,
        }
    }

    /// The embedded payload at `path`: a `!std/enc/<enc>`-typed
    /// line's base64url value, decoded — `(enc, bytes)`. `None` for
    /// non-embed types or an undecodable payload.
    pub fn embed(&self, path: &str) -> Option<(&'d str, Vec<u8>)> {
        let t = self.typed(path)?;
        let enc = t.type_name.strip_prefix("std/enc/")?;
        Some((enc, crate::b64::b64url_decode(t.value)?))
    }

    pub(crate) fn doc(&self) -> &'d Doc {
        self.doc
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    /// This view rescoped below `sub` (e.g. `"/params"`).
    pub fn view(&self, sub: &str) -> View<'d> {
        View {
            doc: self.doc,
            prefix: self.abs(sub),
        }
    }
}

/// Decode a consumer type from a [`View`] — the read-side twin of
/// building with [`DaivBuilder`](crate::builder::DaivBuilder). The
/// scoping convention: a type decodes from the view that OWNS its
/// fields (`::field` at that view's level), so nested structs and
/// array elements decode by delegating to sub-views.
pub trait FromDaiv: Sized {
    fn from_daiv(view: &View<'_>) -> Result<Self, PipelineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = concat!(
        ".!daiv\n",
        "!str'::method=Post.MultiGet\n",
        "!str'/params::ws=TLO\n",
        "!str'/params/@ruids::0=@a+1\n",
        "!str'/params/@ruids::1=@b+2\n",
        "!str'/data/@posts/0::ruid=@a+1\n",
        "!null'/data/@posts/0::title=\n",
        "!str'/data/@posts/0/@tags::0=infra\n",
        "!str'/data/@posts/1::ruid=@b+2\n",
        "!str'/data/@posts/1::title=hello\n",
    );

    #[test]
    fn scalars_structs_and_elements() {
        let doc = Doc::parse(DOC).unwrap();
        let root = doc.root();
        assert_eq!(root.value("::method"), Some("Post.MultiGet"));
        let params = root.view("/params");
        assert_eq!(params.value("::ws"), Some("TLO"));
        assert_eq!(params.scalars("/@ruids"), vec!["@a+1", "@b+2"]);
        let posts = root.elements("/data/@posts");
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].value("::ruid"), Some("@a+1"));
        assert!(posts[0].is_null("::title"));
        assert_eq!(posts[0].value("::title"), None);
        assert_eq!(posts[0].scalars("/@tags"), vec!["infra"]);
        assert_eq!(posts[1].value("::title"), Some("hello"));
        assert_eq!(root.value("::absent"), None);
        assert!(!root.is_null("::absent"));
    }

    #[test]
    fn from_daiv_decoding() {
        struct Params {
            ws: String,
            ruids: Vec<String>,
        }
        impl FromDaiv for Params {
            fn from_daiv(v: &View<'_>) -> Result<Self, PipelineError> {
                Ok(Params {
                    ws: v.required("::ws")?.to_string(),
                    ruids: v.scalars("/@ruids").iter().map(|s| s.to_string()).collect(),
                })
            }
        }
        let doc = Doc::parse(DOC).unwrap();
        let p = Params::from_daiv(&doc.root().view("/params")).unwrap();
        assert_eq!(p.ws, "TLO");
        assert_eq!(p.ruids.len(), 2);
        let missing = Params::from_daiv(&doc.root().view("/nowhere"));
        assert!(missing.is_err());
    }

    #[test]
    fn text_and_embed_round_trip() {
        use crate::builder::DaivBuilder;
        let mut b = DaivBuilder::new();
        b.leaf_text("::body", "<p>line one</p>\n<p>line two</p>", None)
            .unwrap();
        b.leaf_embed("::attachment", "html", b"<p>|:|</p>", None)
            .unwrap();
        let out = b.finish();
        assert!(out.contains("!text'::body=<p>line one</p>|:|<p>line two</p>\n"), "{out}");
        assert!(out.contains("!std/enc/html'::attachment="), "{out}");
        // The canonical form reads back losslessly.
        let doc = Doc::parse(&b.finish_daiv()).unwrap();
        let root = doc.root();
        assert_eq!(
            root.text("::body").as_deref(),
            Some("<p>line one</p>\n<p>line two</p>")
        );
        let (enc, bytes) = root.embed("::attachment").unwrap();
        assert_eq!(enc, "html");
        assert_eq!(bytes, b"<p>|:|</p>");
        // Content the separator cannot carry is refused toward the
        // embed route; CRLF is refused toward normalization.
        let mut b2 = DaivBuilder::new();
        assert!(b2.leaf_text("::x", "a|:|b", None).is_err());
        assert!(b2.leaf_text("::x", "a\r\nb", None).is_err());
    }

    #[test]
    fn rejects_non_daiv() {
        assert!(Doc::parse(".!kaiv\nhost=x\n").is_err());
    }
}
