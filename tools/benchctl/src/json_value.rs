use num_traits::ToPrimitive;
use serde_json::{Number, Value};

fn numeric_integer(number: &Number, convert_float: impl FnOnce(f64) -> Option<i64>) -> Option<i64> {
    if let Some(value) = number.as_i64() {
        Some(value)
    } else if let Some(value) = number.as_u64() {
        i64::try_from(value).ok()
    } else {
        convert_float(number.as_f64()?)
    }
}

/// Integer-compatible JSON number or string, rejecting fractional and out-of-range values.
pub fn exact_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => numeric_integer(number, |value| {
            if value.fract() == 0.0 {
                value.to_i64()
            } else {
                None
            }
        }),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

/// Integer coercion for imported Python-style reports: truncate only finite in-range numbers.
pub fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Number(number) => numeric_integer(number, |value| value.to_i64()),
        Value::String(value) => value.trim().parse().ok(),
        _ => None,
    }
}

pub fn py_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

pub fn json_truthy(value: Option<&Value>) -> bool {
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
    use super::{exact_i64, json_truthy, py_int, py_string};
    use serde_json::{Value, json};

    #[test]
    fn integer_coercion_checks_the_exclusive_float_upper_bound() {
        let too_large = json!(9_223_372_036_854_775_808.0_f64);
        let too_small = json!(-9_223_372_036_854_777_856.0_f64);
        for value in [too_large, too_small, json!(u64::MAX), Value::Null] {
            assert_eq!(exact_i64(&value), None);
            assert_eq!(py_int(&value), None);
        }
        assert_eq!(exact_i64(&json!(i64::MAX)), Some(i64::MAX));
        assert_eq!(
            exact_i64(&json!(-9_223_372_036_854_775_808.0_f64)),
            Some(i64::MIN)
        );
        assert_eq!(exact_i64(&json!(3.5)), None);
        assert_eq!(py_int(&json!(3.5)), Some(3));
        assert_eq!(py_int(&json!(-3.5)), Some(-3));
        assert_eq!(py_int(&json!(true)), Some(1));
        assert_eq!(exact_i64(&json!(true)), None);
    }

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
