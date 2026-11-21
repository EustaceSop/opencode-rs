use serde::Serializer;

const MAX_SAFE: f64 = 9_007_199_254_740_992.0;

// Whole-valued floats are emitted as JSON integers, as JSON.stringify does.
pub fn ser<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < MAX_SAFE {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

pub fn ser_opt<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => ser(value, serializer),
        None => serializer.serialize_none(),
    }
}
