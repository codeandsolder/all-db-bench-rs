use serde_json::Value;

pub(crate) fn py_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

pub(crate) fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::{json_truthy, py_string};
    use serde_json::{Value, json};

    #[test]
    fn python_string_matches_legacy_scalar_rendering() {
        assert_eq!(py_string(&Value::Null), "None");
        assert_eq!(py_string(&json!(true)), "True");
        assert_eq!(py_string(&json!(false)), "False");
        assert_eq!(py_string(&json!(42)), "42");
        assert_eq!(py_string(&json!("hello")), "hello");
        assert_eq!(py_string(&json!([1, 2])), "[1,2]");
        assert_eq!(py_string(&json!({"a": 1})), r#"{"a":1}"#);
    }

    #[test]
    fn json_truthiness_matches_python_style_container_truthiness() {
        assert!(!json_truthy(None));
        assert!(!json_truthy(Some(&Value::Null)));
        assert!(!json_truthy(Some(&json!(false))));
        assert!(!json_truthy(Some(&json!(0))));
        assert!(!json_truthy(Some(&json!(""))));
        assert!(!json_truthy(Some(&json!([]))));
        assert!(!json_truthy(Some(&json!({}))));
        assert!(json_truthy(Some(&json!(true))));
        assert!(json_truthy(Some(&json!(1))));
        assert!(json_truthy(Some(&json!("x"))));
        assert!(json_truthy(Some(&json!([0]))));
        assert!(json_truthy(Some(&json!({"x": 0}))));
    }
}
