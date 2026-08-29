//! 全 object 階層で duplicate member を拒否する。未知 field は後段が無視する。

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::error::{ErrorCode, GateError};

pub fn parse_object_no_dup(bytes: &[u8]) -> Result<Value, GateError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value =
        NoDupValue::deserialize(&mut de).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    de.end()
        .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    if !value.0.is_object() {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(value.0)
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
