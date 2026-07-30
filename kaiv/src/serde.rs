//! serde support (`--features serde`): kaiv as a serde data format.
//!
//! Any `#[derive(Serialize)]` type serializes straight to canonical
//! `.daiv` lines through [`serialize_into`] — no intermediate value
//! tree — and any `#[derive(Deserialize)]` type reads back out of a
//! parsed [`Doc`](crate::doc::Doc) view through [`from_view`]. The
//! mapping mirrors the canonical namepath model:
//!
//! - struct / map fields → `{prefix}::field` scalars,
//!   `{prefix}/field` nested namespaces;
//! - `Vec<scalar>` → `{prefix}/@field::N` vector lines;
//! - `Vec<struct>` → `{prefix}/@field/N::…` element runs;
//! - `Option::None` / unit → `!null` lines; integers `!int`, floats
//!   `!float`, bools `!bool`, strings `!str` — with multi-line
//!   strings riding `!text` (`|:|` separators) and separator
//!   collisions the `std/enc` embed channel.
//!
//! Where a field's wire type must be a NAMED head (a union
//! discriminant such as `!null|forum/core/flair`, or `!text` for
//! single-line values under a text union), pass it in [`Heads`] —
//! serde sees only the Rust type, and unions are nominal.
//!
//! An EMPTY collection has no wire representation (kaiv collections
//! are never themselves required — an empty array is simply
//! absent), so give collection fields `#[serde(default)]`; absent
//! `Option` fields come back `None` either way.
//!
//! Deserialization is driven by the TARGET type, not the line
//! annotations: an untyped authored `limit=50` decodes into an
//! `i64` field just fine. Annotations matter only for
//! self-describing targets (`deserialize_any`, e.g. into a JSON
//! value) and the null/text/embed interpretations.

use crate::builder::DaivBuilder;
use crate::doc::{Doc, View};
use crate::error::PipelineError;
use ::serde::de::{
    DeserializeSeed, Deserializer as DeTrait, IntoDeserializer, MapAccess, SeqAccess, Visitor,
};
use ::serde::ser::{
    Serialize, SerializeMap, SerializeSeq, SerializeStruct, Serializer as SerTrait,
};
use std::fmt;

// ------------------------------------------------------------- error

/// The serde-facing error: a message, convertible from/to
/// [`PipelineError`].
#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl ::serde::ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}
impl ::serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}
impl From<PipelineError> for Error {
    fn from(e: PipelineError) -> Self {
        Error(e.to_string())
    }
}
impl From<Error> for PipelineError {
    fn from(e: Error) -> Self {
        PipelineError::Other(e.0)
    }
}

// ------------------------------------------------------------- heads

/// A field's wire-type override (matched by field name, wherever the
/// field appears).
#[derive(Clone, Copy)]
pub enum Head {
    /// Emit string values as `!text` (single- or multi-line), with
    /// the named `std/enc` payload kind on separator collision.
    Text { embed: &'static str },
    /// Emit string values under this named type — the union
    /// discriminant (`forum/core/flair`, `std/time/datetime`, …).
    Named(&'static str),
}

/// Field-name → [`Head`] overrides.
pub type Heads<'h> = &'h [(&'h str, Head)];

fn head_for<'h>(heads: Heads<'h>, field: &str) -> Option<Head> {
    heads.iter().find(|(f, _)| *f == field).map(|(_, h)| *h)
}

// -------------------------------------------------------- serializer

/// Serialize `value` as canonical lines under `prefix` (e.g.
/// `"/data"`), appended to the builder.
pub fn serialize_into<T: Serialize>(
    b: &mut DaivBuilder,
    prefix: &str,
    heads: Heads,
    value: &T,
) -> Result<(), PipelineError> {
    value
        .serialize(Ser {
            b,
            heads,
            hint: None,
            scalar: format!("{prefix}::value"),
            container: prefix.to_string(),
            seq: format!("{prefix}/@value"),
        })
        .map_err(PipelineError::from)
}

/// One serialization position: the three namepath spellings the
/// value could occupy, resolved by what it turns out to be.
struct Ser<'a, 'h> {
    b: &'a mut DaivBuilder,
    heads: Heads<'h>,
    hint: Option<Head>,
    /// The namepath when the value is a scalar (`P::f`).
    scalar: String,
    /// The prefix when it is a struct/map (`P/f`).
    container: String,
    /// The array base when it is a sequence (`P/@f`).
    seq: String,
}

impl<'a, 'h> Ser<'a, 'h> {
    fn leaf(self, type_name: &str, value: &str) -> Result<(), Error> {
        self.b.leaf(&self.scalar, type_name, value, None)?;
        Ok(())
    }

    fn string(self, s: &str) -> Result<(), Error> {
        if let Some(Head::Named(name)) = self.hint {
            self.b.leaf(&self.scalar, name, s, None)?;
            return Ok(());
        }
        let text_embed = match self.hint {
            Some(Head::Text { embed }) => Some(embed),
            _ => None,
        };
        if text_embed.is_some() || s.contains('\n') || s.contains('\r') {
            let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
            if self.b.leaf_text(&self.scalar, &normalized, None).is_err() {
                self.b.leaf_embed(
                    &self.scalar,
                    text_embed.unwrap_or("plain"),
                    normalized.as_bytes(),
                    None,
                )?;
            }
        } else if self.b.leaf(&self.scalar, "str", s, None).is_err() {
            // Flat-line rules (leading `$`, NUL): embed verbatim.
            self.b.leaf_embed(&self.scalar, "plain", s.as_bytes(), None)?;
        }
        Ok(())
    }
}

macro_rules! ser_int {
    ($($m:ident: $t:ty),*) => {$(
        fn $m(self, v: $t) -> Result<(), Error> {
            self.leaf("int", &v.to_string())
        }
    )*};
}

impl<'a, 'h> SerTrait for Ser<'a, 'h> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = SerSeq<'a, 'h>;
    type SerializeTuple = SerSeq<'a, 'h>;
    type SerializeTupleStruct = SerSeq<'a, 'h>;
    type SerializeTupleVariant = ::serde::ser::Impossible<(), Error>;
    type SerializeMap = SerFields<'a, 'h>;
    type SerializeStruct = SerFields<'a, 'h>;
    type SerializeStructVariant = ::serde::ser::Impossible<(), Error>;

    ser_int!(serialize_i8: i8, serialize_i16: i16, serialize_i32: i32, serialize_i64: i64,
             serialize_u8: u8, serialize_u16: u16, serialize_u32: u32, serialize_u64: u64);

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.leaf("bool", if v { "true" } else { "false" })
    }
    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.leaf("float", &v.to_string())
    }
    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.leaf("float", &v.to_string())
    }
    fn serialize_char(self, v: char) -> Result<(), Error> {
        self.string(&v.to_string())
    }
    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.string(v)
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        self.b.leaf_embed(&self.scalar, "bin", v, None)?;
        Ok(())
    }
    fn serialize_none(self) -> Result<(), Error> {
        self.leaf("null", "")
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<(), Error> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), Error> {
        self.leaf("null", "")
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> {
        self.leaf("null", "")
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<(), Error> {
        self.string(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<(), Error> {
        Err(Error(format!("kaiv: enum data variants unsupported ({name})")))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<SerSeq<'a, 'h>, Error> {
        Ok(SerSeq {
            b: self.b,
            heads: self.heads,
            base: self.seq,
            idx: 0,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<SerSeq<'a, 'h>, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        len: usize,
    ) -> Result<SerSeq<'a, 'h>, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error(format!("kaiv: enum data variants unsupported ({name})")))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<SerFields<'a, 'h>, Error> {
        Ok(SerFields {
            b: self.b,
            heads: self.heads,
            prefix: self.container,
            key: None,
        })
    }
    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<SerFields<'a, 'h>, Error> {
        Ok(SerFields {
            b: self.b,
            heads: self.heads,
            prefix: self.container,
            key: None,
        })
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error(format!("kaiv: enum data variants unsupported ({name})")))
    }
}

/// Struct/map serialization: each field spawns a child [`Ser`] with
/// the three spellings derived from this prefix.
struct SerFields<'a, 'h> {
    b: &'a mut DaivBuilder,
    heads: Heads<'h>,
    prefix: String,
    key: Option<String>,
}

impl<'a, 'h> SerFields<'a, 'h> {
    fn field<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> Result<(), Error> {
        value.serialize(Ser {
            b: self.b,
            heads: self.heads,
            hint: head_for(self.heads, key),
            scalar: format!("{}::{key}", self.prefix),
            container: format!("{}/{key}", self.prefix),
            seq: format!("{}/@{key}", self.prefix),
        })
    }
}

impl<'a, 'h> SerializeStruct for SerFields<'a, 'h> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.field(key, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<'a, 'h> SerializeMap for SerFields<'a, 'h> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        let mut sink = KeyText(None);
        key.serialize(&mut sink)?;
        self.key = Some(sink.0.ok_or_else(|| Error("kaiv: map key must be a string".into()))?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| Error("kaiv: map value before key".into()))?;
        self.field(&key, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

/// Sequence serialization: element N resolves to the vector spelling
/// (`base::N`) for scalars and the element-run spelling (`base/N`)
/// for structs/maps.
struct SerSeq<'a, 'h> {
    b: &'a mut DaivBuilder,
    heads: Heads<'h>,
    base: String,
    idx: usize,
}

impl<'a, 'h> SerializeSeq for SerSeq<'a, 'h> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let i = self.idx;
        self.idx += 1;
        value.serialize(Ser {
            b: self.b,
            heads: self.heads,
            hint: None,
            scalar: format!("{}::{i}", self.base),
            container: format!("{}/{i}", self.base),
            seq: format!("{}/{i}/@value", self.base),
        })
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<'a, 'h> ::serde::ser::SerializeTuple for SerSeq<'a, 'h> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<'a, 'h> ::serde::ser::SerializeTupleStruct for SerSeq<'a, 'h> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

/// A serializer that accepts exactly one string (map keys).
struct KeyText(Option<String>);

impl<'a> SerTrait for &'a mut KeyText {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = ::serde::ser::Impossible<(), Error>;
    type SerializeTuple = ::serde::ser::Impossible<(), Error>;
    type SerializeTupleStruct = ::serde::ser::Impossible<(), Error>;
    type SerializeTupleVariant = ::serde::ser::Impossible<(), Error>;
    type SerializeMap = ::serde::ser::Impossible<(), Error>;
    type SerializeStruct = ::serde::ser::Impossible<(), Error>;
    type SerializeStructVariant = ::serde::ser::Impossible<(), Error>;

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.0 = Some(v.to_string());
        Ok(())
    }

    ::serde::serde_if_integer128! {
        fn serialize_i128(self, _: i128) -> Result<(), Error> {
            Err(Error("kaiv: map key must be a string".into()))
        }
        fn serialize_u128(self, _: u128) -> Result<(), Error> {
            Err(Error("kaiv: map key must be a string".into()))
        }
    }

    fn serialize_bool(self, _: bool) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_i8(self, _: i8) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_i16(self, _: i16) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_i32(self, _: i32) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_i64(self, _: i64) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_u8(self, _: u8) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_u16(self, _: u16) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_u32(self, _: u32) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_u64(self, _: u64) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_f32(self, _: f32) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_f64(self, _: f64) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_char(self, v: char) -> Result<(), Error> { self.serialize_str(&v.to_string()) }
    fn serialize_bytes(self, _: &[u8]) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_none(self) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<(), Error> { v.serialize(self) }
    fn serialize_unit(self) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_unit_variant(self, _: &'static str, _: u32, v: &'static str) -> Result<(), Error> { self.serialize_str(v) }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<(), Error> { v.serialize(self) }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, _: u32, _: &'static str, _: &T) -> Result<(), Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeTupleStruct, Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeTupleVariant, Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Error> { Err(Error("kaiv: map key must be a string".into())) }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeStructVariant, Error> { Err(Error("kaiv: map key must be a string".into())) }
}

// ------------------------------------------------------ deserializer

/// Deserialize `T` from the lines under a [`View`] — the typed twin
/// of walking the view by hand.
pub fn from_view<'de, T: ::serde::Deserialize<'de>>(view: &View<'_>) -> Result<T, Error> {
    T::deserialize(De {
        doc: view.doc(),
        prefix: view.prefix().to_string(),
    })
}

/// Deserialize `T` from a parsed document at a namepath prefix
/// (`""` for the root, `"/params"`, …).
pub fn from_doc<'de, T: ::serde::Deserialize<'de>>(
    doc: &Doc,
    prefix: &str,
) -> Result<T, Error> {
    T::deserialize(De {
        doc,
        prefix: prefix.to_string(),
    })
}

/// One deserialization position: the subtree at `prefix`.
struct De<'a> {
    doc: &'a Doc,
    prefix: String,
}

impl<'a> De<'a> {
    /// The scalar line at exactly `prefix` (a `P::f` position).
    fn scalar(&self) -> Option<(&'a str, &'a str)> {
        self.doc.line_at(&self.prefix)
    }

    /// Child names directly under `prefix`, in first-seen order:
    /// `(name, is_array)`.
    fn children(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = Vec::new();
        let scalar_pfx = format!("{}::", self.prefix);
        let nested_pfx = format!("{}/", self.prefix);
        for np in self.doc.namepaths() {
            let (name, is_array) = if let Some(rest) = np.strip_prefix(scalar_pfx.as_str()) {
                if rest.is_empty() || rest.contains(['/', ':']) {
                    continue;
                }
                (rest.to_string(), false)
            } else if let Some(rest) = np.strip_prefix(nested_pfx.as_str()) {
                let seg = rest
                    .split(['/', ':'])
                    .next()
                    .unwrap_or("")
                    .to_string();
                if seg.is_empty() {
                    continue;
                }
                match seg.strip_prefix('@') {
                    Some(a) => (a.to_string(), true),
                    None => (seg, false),
                }
            } else {
                continue;
            };
            if !out.iter().any(|(n, _)| *n == name) {
                out.push((name, is_array));
            }
        }
        out
    }

    fn has_field(&self, f: &str) -> bool {
        let scalar = format!("{}::{f}", self.prefix);
        let nested = format!("{}/{f}", self.prefix);
        let arr = format!("{}/@{f}", self.prefix);
        self.doc.namepaths().any(|np| {
            np == scalar
                || np.starts_with(nested.as_str())
                    && np[nested.len()..].starts_with(['/', ':'])
                || np.starts_with(arr.as_str()) && np[arr.len()..].starts_with(['/', ':'])
        })
    }

    fn field(&self, f: &str) -> De<'a> {
        De {
            doc: self.doc,
            prefix: format!("{}::{f}", self.prefix),
        }
    }

    /// The element positions of the array at `{prefix minus ::f}` —
    /// used from a field position: `P::f` → base `P/@f`.
    fn array_base(&self) -> Option<String> {
        let (head, f) = self.prefix.rsplit_once("::")?;
        Some(format!("{head}/@{f}"))
    }

    fn array_len(&self, base: &str) -> usize {
        let scalar_pfx = format!("{base}::");
        let run_pfx = format!("{base}/");
        let mut max: Option<usize> = None;
        for np in self.doc.namepaths() {
            let idx = if let Some(rest) = np.strip_prefix(scalar_pfx.as_str()) {
                rest.parse::<usize>().ok()
            } else if let Some(rest) = np.strip_prefix(run_pfx.as_str()) {
                rest.split(['/', ':']).next().and_then(|s| s.parse().ok())
            } else {
                None
            };
            if let Some(i) = idx {
                max = Some(max.map_or(i, |m: usize| m.max(i)));
            }
        }
        max.map_or(0, |m| m + 1)
    }

    fn parse_scalar<T: std::str::FromStr>(&self, what: &str) -> Result<T, Error> {
        let (_, v) = self
            .scalar()
            .ok_or_else(|| Error(format!("kaiv: missing {what} at {}", self.prefix)))?;
        v.parse()
            .map_err(|_| Error(format!("kaiv: {} is not {what}: {v}", self.prefix)))
    }
}

macro_rules! de_int {
    ($($m:ident => $visit:ident: $t:ty),*) => {$(
        fn $m<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            visitor.$visit(self.parse_scalar::<$t>("an integer")?)
        }
    )*};
}

impl<'de, 'a> DeTrait<'de> for De<'a> {
    type Error = Error;

    de_int!(deserialize_i8 => visit_i8: i8, deserialize_i16 => visit_i16: i16,
            deserialize_i32 => visit_i32: i32, deserialize_i64 => visit_i64: i64,
            deserialize_u8 => visit_u8: u8, deserialize_u16 => visit_u16: u16,
            deserialize_u32 => visit_u32: u32, deserialize_u64 => visit_u64: u64);

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let (_, v) = self
            .scalar()
            .ok_or_else(|| Error(format!("kaiv: missing bool at {}", self.prefix)))?;
        match v {
            "true" => visitor.visit_bool(true),
            "false" => visitor.visit_bool(false),
            other => Err(Error(format!("kaiv: {} is not a bool: {other}", self.prefix))),
        }
    }
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f32(self.parse_scalar::<f32>("a float")?)
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f64(self.parse_scalar::<f64>("a float")?)
    }
    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let s: String = string_of(&self)?;
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => visitor.visit_char(c),
            _ => Err(Error(format!("kaiv: {} is not one char", self.prefix))),
        }
    }
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_string(string_of(&self)?)
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_string(string_of(&self)?)
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_byte_buf(bytes_of(&self)?)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_byte_buf(bytes_of(&self)?)
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.scalar() {
            Some(("null", _)) => visitor.visit_none(),
            Some(_) => visitor.visit_some(self),
            None => {
                // A nested/array presence still means Some.
                let nested = De {
                    doc: self.doc,
                    prefix: self.prefix.clone(),
                };
                if has_subtree(&nested) {
                    visitor.visit_some(self)
                } else {
                    visitor.visit_none()
                }
            }
        }
    }
    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let base = self
            .array_base()
            .ok_or_else(|| Error(format!("kaiv: no array position at {}", self.prefix)))?;
        let len = self.array_len(&base);
        visitor.visit_seq(SeqDe {
            doc: self.doc,
            base,
            len,
            idx: 0,
        })
    }
    fn deserialize_tuple<V: Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        // A map/struct position: the container spelling of this
        // field position.
        let container = container_prefix(&self.prefix);
        let inner = De {
            doc: self.doc,
            prefix: container,
        };
        let fields: Vec<(String, bool)> = inner.children();
        visitor.visit_map(MapDe {
            doc: self.doc,
            prefix: inner.prefix,
            fields,
            idx: 0,
        })
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let container = container_prefix(&self.prefix);
        let inner = De {
            doc: self.doc,
            prefix: container,
        };
        let present: Vec<(String, bool)> = fields
            .iter()
            .filter(|f| inner.has_field(f))
            .map(|f| (f.to_string(), false))
            .collect();
        visitor.visit_map(MapDe {
            doc: self.doc,
            prefix: inner.prefix,
            fields: present,
            idx: 0,
        })
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_enum(string_of(&self)?.into_deserializer())
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_string(visitor)
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Some((type_name, value)) = self.scalar() {
            return match type_name {
                "null" => visitor.visit_unit(),
                "bool" => match value {
                    "true" => visitor.visit_bool(true),
                    _ => visitor.visit_bool(false),
                },
                "int" => match value.parse::<i64>() {
                    Ok(n) => visitor.visit_i64(n),
                    Err(_) => visitor.visit_string(value.to_string()),
                },
                "float" => match value.parse::<f64>() {
                    Ok(n) => visitor.visit_f64(n),
                    Err(_) => visitor.visit_string(value.to_string()),
                },
                "text" => visitor.visit_string(value.replace("|:|", "\n")),
                _ => visitor.visit_string(value.to_string()),
            };
        }
        // No scalar line: an array position (if index lines exist),
        // else a map.
        if let Some(base) = self.array_base() {
            let len = self.array_len(&base);
            if len > 0 {
                return visitor.visit_seq(SeqDe {
                    doc: self.doc,
                    base,
                    len,
                    idx: 0,
                });
            }
        }
        self.deserialize_map(visitor)
    }
}

/// A field position `P::f` → the container spelling `P/f`; a root
/// prefix stays itself.
fn container_prefix(prefix: &str) -> String {
    match prefix.rsplit_once("::") {
        Some((head, f)) if !f.contains('/') => format!("{head}/{f}"),
        _ => prefix.to_string(),
    }
}

fn has_subtree(d: &De<'_>) -> bool {
    let container = container_prefix(&d.prefix);
    let arr = match d.prefix.rsplit_once("::") {
        Some((head, f)) => format!("{head}/@{f}"),
        None => return false,
    };
    d.doc.namepaths().any(|np| {
        (np.starts_with(container.as_str()) && np[container.len()..].starts_with(['/', ':']))
            || (np.starts_with(arr.as_str()) && np[arr.len()..].starts_with(['/', ':']))
    })
}

fn string_of(d: &De<'_>) -> Result<String, Error> {
    let (type_name, value) = d
        .scalar()
        .ok_or_else(|| Error(format!("kaiv: missing string at {}", d.prefix)))?;
    Ok(match type_name {
        "text" => value.replace("|:|", "\n"),
        name if name.starts_with("std/enc/") => {
            let bytes = crate::b64::b64url_decode(value)
                .ok_or_else(|| Error(format!("kaiv: bad base64url at {}", d.prefix)))?;
            String::from_utf8(bytes)
                .map_err(|_| Error(format!("kaiv: embed at {} is not UTF-8", d.prefix)))?
        }
        _ => value.to_string(),
    })
}

fn bytes_of(d: &De<'_>) -> Result<Vec<u8>, Error> {
    let (type_name, value) = d
        .scalar()
        .ok_or_else(|| Error(format!("kaiv: missing bytes at {}", d.prefix)))?;
    if type_name.starts_with("std/enc/") || type_name == "b64" {
        crate::b64::b64url_decode(value)
            .ok_or_else(|| Error(format!("kaiv: bad base64url at {}", d.prefix)))
    } else {
        Ok(value.as_bytes().to_vec())
    }
}

struct SeqDe<'a> {
    doc: &'a Doc,
    base: String,
    len: usize,
    idx: usize,
}

impl<'de, 'a> SeqAccess<'de> for SeqDe<'a> {
    type Error = Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        if self.idx >= self.len {
            return Ok(None);
        }
        let i = self.idx;
        self.idx += 1;
        // Scalar elements live at `base::i`; element runs at `base/i`
        // — present the position whose lines exist.
        let scalar = format!("{}::{i}", self.base);
        let prefix = if self.doc.line_at(&scalar).is_some() {
            scalar
        } else {
            // The struct-position spelling: a synthetic field position
            // whose container resolves to `base/i`.
            format!("{}::{i}", self.base)
        };
        // container_prefix("base::i") gives "base/i" for the nested
        // case, and the scalar read hits `base::i` directly.
        seed.deserialize(De {
            doc: self.doc,
            prefix,
        })
        .map(Some)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.len - self.idx)
    }
}

struct MapDe<'a> {
    doc: &'a Doc,
    prefix: String,
    fields: Vec<(String, bool)>,
    idx: usize,
}

impl<'de, 'a> MapAccess<'de> for MapDe<'a> {
    type Error = Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        if self.idx >= self.fields.len() {
            return Ok(None);
        }
        let name = self.fields[self.idx].0.clone();
        seed.deserialize(name.into_deserializer()).map(Some)
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let (name, _) = self.fields[self.idx].clone();
        self.idx += 1;
        seed.deserialize(De {
            doc: self.doc,
            prefix: format!("{}::{name}", self.prefix),
        })
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len() - self.idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Post {
        id: String,
        level: i32,
        flair: Option<String>,
        title: Option<String>,
        body: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        locked: bool,
        score: f64,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Reply {
        posts: Vec<Post>,
        n: i64,
    }

    const HEADS: Heads<'static> = &[
        ("body", Head::Text { embed: "html" }),
        ("flair", Head::Named("forum/core/flair")),
    ];

    fn sample() -> Reply {
        Reply {
            posts: vec![
                Post {
                    id: "t3_a1".into(),
                    level: 1,
                    flair: Some("acme".into()),
                    title: Some("hello".into()),
                    body: Some("<p>one</p>\n<p>two</p>".into()),
                    tags: vec!["music".into(), "repair".into()],
                    locked: false,
                    score: 1.5,
                },
                Post {
                    id: "t1_b1".into(),
                    level: 2,
                    flair: None,
                    title: None,
                    body: Some("<p>solo</p>".into()),
                    tags: vec![],
                    locked: true,
                    score: -0.25,
                },
            ],
            n: 2,
        }
    }

    fn emit(value: &Reply) -> String {
        let mut b = DaivBuilder::new();
        serialize_into(&mut b, "/data", HEADS, value).unwrap();
        b.finish_daiv()
    }

    #[test]
    fn typed_round_trip_with_heads() {
        let daiv = emit(&sample());
        // Domain-typed lines, straight from serde.
        assert!(daiv.contains("!forum/core/flair'/data/@posts/0::flair=acme"), "{daiv}");
        assert!(daiv.contains("!text'/data/@posts/0::body=<p>one</p>|:|<p>two</p>"), "{daiv}");
        // The single-line body still rides !text (hinted field).
        assert!(daiv.contains("!text'/data/@posts/1::body=<p>solo</p>"), "{daiv}");
        assert!(daiv.contains("!null'/data/@posts/1::flair="), "{daiv}");
        assert!(daiv.contains("!bool'/data/@posts/1::locked=true"), "{daiv}");
        assert!(daiv.contains("!float'/data/@posts/0::score=1.5"), "{daiv}");
        assert!(daiv.contains("!str'/data/@posts/0/@tags::1=repair"), "{daiv}");
        assert!(daiv.contains("!int'::data/") == false);
        let doc = Doc::parse(&daiv).unwrap();
        let back: Reply = from_doc(&doc, "/data").unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn untyped_authored_params_decode_by_target_type() {
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(rename_all = "camelCase")]
        struct Params {
            ws: String,
            ids: Vec<String>,
            #[serde(default)]
            limit: i64,
            #[serde(default)]
            is_staff: Option<bool>,
        }
        // Untyped (!str) lines — the pre-lift authored form.
        let daiv = concat!(
            ".!daiv\n",
            "!str'/params::ws=TLO\n",
            "!str'/params/@ids::0=t3_a1\n",
            "!str'/params/@ids::1=t1_b2\n",
            "!str'/params::limit=50\n",
        );
        let doc = Doc::parse(daiv).unwrap();
        let p: Params = from_doc(&doc, "/params").unwrap();
        assert_eq!(p.ws, "TLO");
        assert_eq!(p.ids, vec!["t3_a1", "t1_b2"]);
        assert_eq!(p.limit, 50);
        assert_eq!(p.is_staff, None);
    }

    #[test]
    fn self_describing_targets_read_annotations() {
        let daiv = concat!(
            ".!daiv\n",
            "!str'/data::name=web\n",
            "!int'/data::port=8443\n",
            "!bool'/data::on=true\n",
            "!null'/data::note=\n",
            "!text'/data::doc=a|:|b\n",
            "!str'/data/@xs::0=p\n",
            "!int'/data/nested::k=7\n",
        );
        let doc = Doc::parse(daiv).unwrap();
        let v: std::collections::BTreeMap<String, CatchAll> =
            from_doc(&doc, "/data").unwrap();
        assert!(matches!(v["port"], CatchAll::Int(8443)));
        assert!(matches!(v["on"], CatchAll::Bool(true)));
        assert!(matches!(v["note"], CatchAll::Null));
        assert!(matches!(&v["doc"], CatchAll::Str(s) if s == "a\nb"));
        assert!(matches!(&v["xs"], CatchAll::List(l) if l.len() == 1));
        assert!(matches!(&v["nested"], CatchAll::Map(m) if matches!(m["k"], CatchAll::Int(7))));
    }

    #[derive(Deserialize, Debug)]
    #[serde(untagged)]
    enum CatchAll {
        Null,
        Bool(bool),
        Int(i64),
        Float(f64),
        Str(String),
        List(Vec<CatchAll>),
        Map(std::collections::BTreeMap<String, CatchAll>),
    }
}
