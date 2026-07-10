//! Typed argument extraction from the raw JSON arguments object.
//!
//! Every tool receives a `serde_json::Value`. Rather than deriving a struct per
//! tool (which loses the ability to give precise per-field error messages),
//! tools use [`Args`], a thin wrapper that yields typed values with
//! [`ToolError::InvalidArguments`] on any mismatch.

use nebula_mcp_core::ToolError;
use serde_json::Value;

/// A borrowed view over a tool's arguments object.
pub struct Args<'a> {
    value: &'a Value,
}

impl<'a> Args<'a> {
    /// Wrap a JSON value, requiring it to be an object (or null, treated as an
    /// empty object).
    pub fn new(value: &'a Value) -> Result<Self, ToolError> {
        if value.is_object() || value.is_null() {
            Ok(Self { value })
        } else {
            Err(ToolError::InvalidArguments(
                "arguments must be a JSON object".to_string(),
            ))
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.value.get(key).filter(|v| !v.is_null())
    }

    /// Required string field.
    pub fn str(&self, key: &str) -> Result<&str, ToolError> {
        self.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments(format!("missing required string '{key}'")))
    }

    /// Optional string field.
    pub fn opt_str(&self, key: &str) -> Result<Option<&str>, ToolError> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.as_str().map(Some).ok_or_else(|| {
                ToolError::InvalidArguments(format!("field '{key}' must be a string"))
            }),
        }
    }

    /// Optional string with a default.
    pub fn str_or<'b>(&'b self, key: &str, default: &'b str) -> Result<&'b str, ToolError> {
        Ok(self.opt_str(key)?.unwrap_or(default))
    }

    /// Optional boolean with a default.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool, ToolError> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or_else(|| {
                ToolError::InvalidArguments(format!("field '{key}' must be a boolean"))
            }),
        }
    }

    /// Optional unsigned integer.
    pub fn opt_u64(&self, key: &str) -> Result<Option<u64>, ToolError> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| {
                ToolError::InvalidArguments(format!("field '{key}' must be a non-negative integer"))
            }),
        }
    }

    /// Optional unsigned integer with a default.
    pub fn u64_or(&self, key: &str, default: u64) -> Result<u64, ToolError> {
        Ok(self.opt_u64(key)?.unwrap_or(default))
    }

    /// Optional signed integer.
    pub fn opt_i64(&self, key: &str) -> Result<Option<i64>, ToolError> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.as_i64().map(Some).ok_or_else(|| {
                ToolError::InvalidArguments(format!("field '{key}' must be an integer"))
            }),
        }
    }

    /// Required array of strings.
    pub fn str_array(&self, key: &str) -> Result<Vec<String>, ToolError> {
        let arr = self.get(key).and_then(Value::as_array).ok_or_else(|| {
            ToolError::InvalidArguments(format!("missing required string array '{key}'"))
        })?;
        Self::collect_strings(key, arr)
    }

    /// Optional array of strings (absent → empty).
    pub fn opt_str_array(&self, key: &str) -> Result<Vec<String>, ToolError> {
        match self.get(key).and_then(Value::as_array) {
            None => {
                // Distinguish "absent" from "present but wrong type".
                if self.get(key).is_some() {
                    return Err(ToolError::InvalidArguments(format!(
                        "field '{key}' must be an array of strings"
                    )));
                }
                Ok(Vec::new())
            }
            Some(arr) => Self::collect_strings(key, arr),
        }
    }

    fn collect_strings(key: &str, arr: &[Value]) -> Result<Vec<String>, ToolError> {
        arr.iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    ToolError::InvalidArguments(format!(
                        "every element of '{key}' must be a string"
                    ))
                })
            })
            .collect()
    }

    /// Raw access to an optional sub-value.
    pub fn opt_value(&self, key: &str) -> Option<&Value> {
        self.get(key)
    }

    /// Optional map of string → string (used for environment variables).
    pub fn opt_string_map(&self, key: &str) -> Result<Vec<(String, String)>, ToolError> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Object(map)) => {
                let mut out = Vec::with_capacity(map.len());
                for (k, v) in map {
                    let s = v.as_str().ok_or_else(|| {
                        ToolError::InvalidArguments(format!(
                            "value for '{key}.{k}' must be a string"
                        ))
                    })?;
                    out.push((k.clone(), s.to_string()));
                }
                Ok(out)
            }
            Some(_) => Err(ToolError::InvalidArguments(format!(
                "field '{key}' must be an object of string values"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_typed_fields() {
        let v = json!({"path": "a.txt", "recursive": true, "count": 3, "items": ["x", "y"]});
        let a = Args::new(&v).unwrap();
        assert_eq!(a.str("path").unwrap(), "a.txt");
        assert!(a.bool_or("recursive", false).unwrap());
        assert_eq!(a.u64_or("count", 0).unwrap(), 3);
        assert_eq!(a.str_array("items").unwrap(), vec!["x", "y"]);
        assert!(a.opt_str("missing").unwrap().is_none());
    }

    #[test]
    fn wrong_types_error() {
        let v = json!({"path": 5});
        let a = Args::new(&v).unwrap();
        assert!(a.str("path").is_err());
    }

    #[test]
    fn null_arguments_treated_as_empty() {
        let v = Value::Null;
        let a = Args::new(&v).unwrap();
        assert!(a.opt_str("x").unwrap().is_none());
        assert_eq!(a.u64_or("n", 7).unwrap(), 7);
    }

    #[test]
    fn env_map_parses() {
        let v = json!({"env": {"A": "1", "B": "two"}});
        let a = Args::new(&v).unwrap();
        let mut env = a.opt_string_map("env").unwrap();
        env.sort();
        assert_eq!(
            env,
            vec![("A".into(), "1".into()), ("B".into(), "two".into())]
        );
    }
}
