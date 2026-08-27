//! V3 §3.1: 全 object 階層で duplicate member を拒否する。未知 field は後段が無視する。
//! core crate の DTO は使わない。

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

pub fn parse_object_no_dup(bytes: &[u8]) -> Result<Value, JsonError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = NoDupValue::deserialize(&mut de).map_err(|_| JsonError::BadRequest)?;
    de.end().map_err(|_| JsonError::BadRequest)?;
    if !value.0.is_object() {
        return Err(JsonError::BadRequest);
    }
    Ok(value.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonError {
    BadRequest,
}

struct NoDupValue(Value);

impl<'de> Deserialize<'de> for NoDupValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(NoDupVisitor)
    }
}

struct NoDupVisitor;

impl<'de> Visitor<'de> for NoDupVisitor {
    type Value = NoDupValue;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(NoDupValue(Value::Bool(v)))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(NoDupValue(Value::Number(v.into())))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(NoDupValue(Value::Number(v.into())))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let n = serde_json::Number::from_f64(v).ok_or_else(|| E::custom("invalid number"))?;
        Ok(NoDupValue(Value::Number(n)))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(NoDupValue(Value::String(v.to_string())))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(NoDupValue(Value::String(v)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(NoDupValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(NoDupValue(Value::Null))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<NoDupValue>()? {
            items.push(item.0);
        }
        Ok(NoDupValue(Value::Array(items)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = BTreeSet::new();
        let mut obj = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom("duplicate member"));
            }
            let value = map.next_value::<NoDupValue>()?;
            obj.insert(key, value.0);
        }
        Ok(NoDupValue(Value::Object(obj)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_member() {
        let err = parse_object_no_dup(br#"{"a":1,"a":2}"#).unwrap_err();
        assert_eq!(err, JsonError::BadRequest);
    }

    #[test]
    fn accepts_object() {
        let v = parse_object_no_dup(br#"{"a":1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn rejects_array() {
        assert_eq!(
            parse_object_no_dup(b"[1]").unwrap_err(),
            JsonError::BadRequest
        );
    }
}
