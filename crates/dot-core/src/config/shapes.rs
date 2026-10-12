//! Reusable YAML shapes with hand-written `Deserialize` impls. They exist instead of
//! `#[serde(untagged)]` because untagged enums swallow the inner error: a typo inside a long-form
//! mapping would surface as "did not match any variant" instead of "unknown field `srcc`".

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, de::value::MapAccessDeserializer};

/// A mapping with string keys that keeps file order and rejects duplicate keys.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderedMap<V>(pub Vec<(String, V)>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for V_<V> {
            type Value = OrderedMap<V>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a mapping")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<(String, V)> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if out.iter().any(|(k, _)| *k == key) {
                        return Err(de::Error::custom(format!("duplicate key `{key}`")));
                    }
                    let value = map.next_value()?;
                    out.push((key, value));
                }
                Ok(OrderedMap(out))
            }
        }
        d.deserialize_map(V_(PhantomData))
    }
}

impl<V: JsonSchema> JsonSchema for OrderedMap<V> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("Map_of_{}", V::schema_name()).into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "object", "additionalProperties": g.subschema_for::<V>() })
    }
}

/// Types with a one-string shorthand, e.g. `~/.npmrc: npmrc` for `{src: npmrc}`.
pub trait FromScalar: Sized {
    fn from_scalar(s: String) -> Self;
}

/// Either the scalar shorthand of `T` or its full mapping form.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarOr<T>(pub T);

impl<'de, T: FromScalar + DeserializeOwned> Deserialize<'de> for ScalarOr<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_<T>(PhantomData<T>);
        impl<'de, T: FromScalar + DeserializeOwned> Visitor<'de> for V_<T> {
            type Value = ScalarOr<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a mapping")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ScalarOr(T::from_scalar(v.to_string())))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(ScalarOr)
            }
        }
        d.deserialize_any(V_(PhantomData))
    }
}

impl<T: JsonSchema> JsonSchema for ScalarOr<T> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("String_or_{}", T::schema_name()).into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        json_schema!({ "anyOf": [ { "type": "string" }, g.subschema_for::<T>() ] })
    }
}

/// Either a list of names (each with default options) or a mapping of name to options, where an
/// empty value (`~/.cache:`) also means default options.
#[derive(Debug, Clone, PartialEq)]
pub struct ListOrMap<T>(pub Vec<(String, T)>);

impl<'de, T: Default + DeserializeOwned> Deserialize<'de> for ListOrMap<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_<T>(PhantomData<T>);
        impl<'de, T: Default + DeserializeOwned> Visitor<'de> for V_<T> {
            type Value = ListOrMap<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a list of paths or a mapping of path to options")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    out.push((s, T::default()));
                }
                Ok(ListOrMap(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let m = OrderedMap::<Option<T>>::deserialize(MapAccessDeserializer::new(map))?;
                Ok(ListOrMap(
                    m.0.into_iter()
                        .map(|(k, v)| (k, v.unwrap_or_default()))
                        .collect(),
                ))
            }
        }
        d.deserialize_any(V_(PhantomData))
    }
}

impl<T: JsonSchema> JsonSchema for ListOrMap<T> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("List_or_map_of_{}", T::schema_name()).into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        let opts = g.subschema_for::<Option<T>>();
        json_schema!({
            "anyOf": [
                { "type": "array", "items": { "type": "string" } },
                { "type": "object", "additionalProperties": opts }
            ]
        })
    }
}

/// One value or a list of values: `os: macos` or `os: [linux, macos]`.
#[derive(Debug, Clone, PartialEq)]
pub struct OneOrMany(pub Vec<String>);

impl<'de> Deserialize<'de> for OneOrMany {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl<'de> Visitor<'de> for V_ {
            type Value = OneOrMany;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a list of strings")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(OneOrMany(vec![v.to_string()]))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    out.push(s);
                }
                if out.is_empty() {
                    return Err(de::Error::custom("expected at least one value"));
                }
                Ok(OneOrMany(out))
            }
        }
        d.deserialize_any(V_)
    }
}

impl JsonSchema for OneOrMany {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "OneOrMany".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                { "type": "string" },
                { "type": "array", "items": { "type": "string" }, "minItems": 1 }
            ]
        })
    }
}

/// A Unix file mode written as a quoted octal string: `"0700"`, `"644"`, `"0o600"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode(pub u32);

impl Mode {
    pub fn parse(s: &str) -> Result<Mode, String> {
        let digits = s.strip_prefix("0o").unwrap_or(s);
        let ok_len = (3..=5).contains(&digits.len());
        let all_octal = digits.bytes().all(|b| (b'0'..=b'7').contains(&b));
        match u32::from_str_radix(digits, 8) {
            Ok(v) if ok_len && all_octal && v <= 0o7777 => Ok(Mode(v)),
            _ => Err(format!(
                "invalid file mode `{s}`: expected an octal string like \"0700\" or \"0644\""
            )),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl Visitor<'_> for V_ {
            type Value = Mode;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a quoted octal file mode such as \"0700\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Mode, E> {
                Mode::parse(v).map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Mode, E> {
                Err(E::custom(unquoted_mode_message(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Mode, E> {
                self.visit_u64(v.unsigned_abs())
            }
        }
        d.deserialize_any(V_)
    }
}

/// The error for a mode YAML read as an integer. The original spelling is gone by then (`0o700`
/// arrives as 448), so a corrected value is suggested only when it can be inferred: the decimal
/// digits are all octal (`700` -> "0700"), or else the value is itself a valid mode (`0o700` ->
/// "0700").
pub fn unquoted_mode_message(v: u64) -> String {
    let digits = v.to_string();
    let hint = if digits.len() <= 4 && digits.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        Some(format!("0{digits:0>3}"))
    } else if v <= 0o7777 {
        Some(format!("0{v:03o}"))
    } else {
        None
    };
    let mut msg = format!(
        "file mode must be a quoted octal string such as \"0700\"; YAML read this unquoted \
         value as the integer {v}"
    );
    if let Some(h) = hint {
        msg.push_str(&format!(" (did you mean \"{h}\"?)"));
    }
    msg
}

/// A mode as written in the config: a literal, validated while parsing, or a template, rendered
/// and validated at plan time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeSpec {
    Literal(Mode),
    Template(String),
}

impl<'de> Deserialize<'de> for ModeSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl Visitor<'_> for V_ {
            type Value = ModeSpec;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a quoted octal file mode such as \"0700\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ModeSpec, E> {
                if crate::template::is_template(v) {
                    Ok(ModeSpec::Template(v.to_string()))
                } else {
                    Mode::parse(v).map(ModeSpec::Literal).map_err(E::custom)
                }
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ModeSpec, E> {
                Err(E::custom(unquoted_mode_message(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ModeSpec, E> {
                self.visit_u64(v.unsigned_abs())
            }
        }
        d.deserialize_any(V_)
    }
}

impl JsonSchema for Mode {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "Mode".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": MODE_PATTERN,
            "description": "Octal file mode as a quoted string, e.g. \"0700\"."
        })
    }
}

/// Exactly the strings [`Mode::parse`] accepts: optional `0o`, then an octal value of at most
/// 0o7777 written with 3 or 4 digits plus an optional leading zero.
pub const MODE_PATTERN: &str = "^(0o)?0?[0-7]{3,4}$";

impl JsonSchema for ModeSpec {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "Mode".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "anyOf": [
                { "pattern": MODE_PATTERN },
                { "pattern": super::raw::TEMPLATE_PATTERN }
            ],
            "description": "Octal file mode as a quoted string, e.g. \"0700\", or a template."
        })
    }
}

/// Validation run on a mapping right after it is parsed, while the parser still knows where the
/// mapping is, so the error gets a line number.
pub trait Check {
    fn check(&self) -> Result<(), String>;
}

/// A mapping that is `T` and passed `T::check`.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked<T>(pub T);

impl<'de, T: Check + DeserializeOwned> Deserialize<'de> for Checked<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_<T>(PhantomData<T>);
        impl<'de, T: Check + DeserializeOwned> Visitor<'de> for V_<T> {
            type Value = Checked<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a mapping")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let v = T::deserialize(MapAccessDeserializer::new(map))?;
                v.check().map_err(de::Error::custom)?;
                Ok(Checked(v))
            }
        }
        d.deserialize_any(V_(PhantomData))
    }
}

impl<T: JsonSchema> JsonSchema for Checked<T> {
    fn inline_schema() -> bool {
        T::inline_schema()
    }
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }
    fn schema_id() -> Cow<'static, str> {
        T::schema_id()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        T::json_schema(g)
    }
}
