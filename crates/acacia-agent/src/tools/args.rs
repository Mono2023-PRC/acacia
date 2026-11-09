//! Typed argument reads over a tool call's JSON object.
//!
//! This is deliberately not a JSON Schema engine: the schema exists for the model, the reader exists
//! for the tool. Every error names the argument and the type the schema declared, so the model can
//! correct itself (`"radius" must be an integer`).

use serde_json::{Map, Value};

use super::result::Failure;

/// One tool call's arguments.
pub struct Args {
    tool: &'static str,
    map: Map<String, Value>,
}

impl Args {
    /// Accepts a missing or `null` `arguments` (an empty object, as the bridge does) but not a
    /// non-object.
    pub fn new(tool: &'static str, arguments: Option<&Value>) -> Result<Self, Failure> {
        match arguments {
            None | Some(Value::Null) => Ok(Args { tool, map: Map::new() }),
            Some(Value::Object(map)) => Ok(Args { tool, map: map.clone() }),
            Some(_) => Err(Failure::invalid_argument(format!("{tool}: arguments must be a JSON object"))),
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key).filter(|value| !value.is_null())
    }

    /// The raw value for `key`, for an argument whose type is a union (a number **or** a keyword).
    /// Tools must still validate what they read: this is the escape hatch, not a typed read.
    pub fn raw(&self, key: &str) -> Option<&Value> {
        self.get(key)
    }

    /// An integer, accepting `12.0` for `12` (JSON has one number type; the bridge does the same).
    pub fn int(&self, key: &str, default: Option<i64>) -> Result<i64, Failure> {
        match self.get(key) {
            None => default.ok_or_else(|| self.missing(key, "an integer")),
            Some(Value::Number(number)) => number
                .as_i64()
                .or_else(|| number.as_f64().filter(|value| value.fract() == 0.0).map(|value| value as i64))
                .ok_or_else(|| self.wrong(key, "an integer")),
            Some(_) => Err(self.wrong(key, "an integer")),
        }
    }

    /// An integer clamped into `min..=max`.
    pub fn uint(&self, key: &str, default: u64, min: u64, max: u64) -> Result<u64, Failure> {
        let value = self.int(key, Some(default as i64))?;
        if value < min as i64 {
            return Err(Failure::invalid_argument(format!("{key} must be at least {min}")));
        }
        Ok((value as u64).min(max))
    }

    pub fn u8(&self, key: &str, default: u8, min: u8, max: u8) -> Result<u8, Failure> {
        Ok(self.uint(key, u64::from(default), u64::from(min), u64::from(max))? as u8)
    }

    pub fn u32(&self, key: &str, default: u32, min: u32, max: u32) -> Result<u32, Failure> {
        Ok(self.uint(key, u64::from(default), u64::from(min), u64::from(max))? as u32)
    }

    /// A finite number; non-finite values are rejected instead of poisoning the bot's controls.
    pub fn f32(&self, key: &str, default: Option<f32>) -> Result<f32, Failure> {
        match self.get(key) {
            None => default.ok_or_else(|| self.missing(key, "a number")),
            Some(Value::Number(number)) => match number.as_f64() {
                Some(value) if value.is_finite() => Ok(value as f32),
                _ => Err(self.wrong(key, "a finite number")),
            },
            Some(_) => Err(self.wrong(key, "a number")),
        }
    }

    pub fn boolean(&self, key: &str, default: bool) -> Result<bool, Failure> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(self.wrong(key, "a boolean")),
        }
    }

    pub fn string(&self, key: &str) -> Result<Option<String>, Failure> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(self.wrong(key, "a string")),
        }
    }

    pub fn required_string(&self, key: &str) -> Result<String, Failure> {
        self.string(key)?.ok_or_else(|| self.missing(key, "a string"))
    }

    /// A string from a closed set; the schema's `enum` is the contract.
    pub fn variant(&self, key: &str, allowed: &[&'static str], default: Option<&'static str>) -> Result<&'static str, Failure> {
        match self.string(key)? {
            None => default.ok_or_else(|| self.missing(key, "a string")),
            Some(text) => allowed.iter().copied().find(|candidate| *candidate == text).ok_or_else(|| {
                Failure::invalid_argument(format!("{key} must be one of {allowed:?}, not {text:?}"))
            }),
        }
    }

    pub fn string_list(&self, key: &str) -> Result<Option<Vec<String>>, Failure> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::String(text) => out.push(text.clone()),
                        _ => return Err(self.wrong(key, "a list of strings")),
                    }
                }
                Ok(Some(out))
            }
            Some(_) => Err(self.wrong(key, "a list of strings")),
        }
    }

    /// A block position as `[x,y,z]` or `{"x":..,"y":..,"z":..}`.
    pub fn block_pos(&self, key: &str) -> Result<Option<[i32; 3]>, Failure> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) if items.len() == 3 => {
                let mut out = [0i32; 3];
                for (index, item) in items.iter().enumerate() {
                    let value = item
                        .as_f64()
                        .filter(|value| value.is_finite() && value.fract() == 0.0)
                        .ok_or_else(|| self.wrong(key, "three integers [x,y,z]"))?;
                    out[index] = value as i32;
                }
                Ok(Some(out))
            }
            Some(Value::Object(map)) => {
                let axis = |name: &str| {
                    map.get(name)
                        .and_then(Value::as_f64)
                        .filter(|value| value.is_finite())
                        .map(|value| value as i32)
                        .ok_or_else(|| self.wrong(key, "an object with x, y and z"))
                };
                Ok(Some([axis("x")?, axis("y")?, axis("z")?]))
            }
            Some(_) => Err(self.wrong(key, "[x,y,z] or {x,y,z}")),
        }
    }

    pub fn required_block_pos(&self, key: &str) -> Result<[i32; 3], Failure> {
        self.block_pos(key)?.ok_or_else(|| self.missing(key, "[x,y,z]"))
    }

    /// A pair `[i32;2]`, used by `move_to`'s `xz`.
    pub fn i32_pair(&self, key: &str) -> Result<Option<[i32; 2]>, Failure> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) if items.len() == 2 => {
                let read = |value: &Value| {
                    value
                        .as_f64()
                        .filter(|value| value.is_finite() && value.fract() == 0.0)
                        .map(|value| value as i32)
                        .ok_or_else(|| self.wrong(key, "two integers [x,z]"))
                };
                Ok(Some([read(&items[0])?, read(&items[1])?]))
            }
            Some(_) => Err(self.wrong(key, "[x,z]")),
        }
    }

    /// An optional integer in `min..=max`.
    pub fn optional_u8(&self, key: &str, min: u8, max: u8) -> Result<Option<u8>, Failure> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => Ok(Some(self.u8(key, min, min, max)?)),
        }
    }

    /// An optional finite number.
    pub fn optional_f32(&self, key: &str) -> Result<Option<f32>, Failure> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => Ok(Some(self.f32(key, None)?)),
        }
    }

    /// An optional `[dx,dy,dz]` offset, used by `block`.
    pub fn offset(&self, key: &str) -> Result<Option<[i32; 3]>, Failure> {
        self.block_pos(key)
    }

    pub fn tool(&self) -> &'static str {
        self.tool
    }

    fn missing(&self, key: &str, expected: &str) -> Failure {
        Failure::invalid_argument(format!("{}: missing required argument {key:?} ({expected})", self.tool))
    }

    fn wrong(&self, key: &str, expected: &str) -> Failure {
        Failure::invalid_argument(format!("{key} must be {expected}"))
    }
}
