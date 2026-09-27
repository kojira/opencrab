use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    value_bytes(&serde_json::to_value(value)?)
}

pub fn hash<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(hex(&Sha256::digest(bytes(value)?)))
}

pub fn value_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => {
            let rendered = number.to_string();
            if rendered.contains(['e', 'E']) || rendered == "-0" {
                bail!("non-canonical manifest number");
            }
            out.extend_from_slice(rendered.as_bytes());
        }
        Value::String(text) => out.extend_from_slice(serde_json::to_string(text)?.as_bytes()),
        Value::Array(values) => {
            out.push(b'[');
            for (index, item) in values.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            out.push(b'{');
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key)?.as_bytes());
                out.push(b':');
                write_value(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut text, "{byte:02x}").expect("string write");
    }
    text
}

pub fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
