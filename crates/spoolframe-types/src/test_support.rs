//! Checks shared by the modules' tests.

use std::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// `value` as JSON, then each of `fields` left out in turn: every one must
/// be refused as missing (PX-15), not filled in.
pub(crate) fn assert_each_required<T>(value: &T, fields: &[&str])
where
    T: Serialize + DeserializeOwned + Debug,
{
    let json = serde_json::to_value(value).unwrap();
    for field in fields {
        let mut without = json.clone();
        assert!(
            without.as_object_mut().unwrap().remove(*field).is_some(),
            "{field} is not a key of {json}"
        );
        let error = serde_json::from_value::<T>(without).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("missing field `{field}`")),
            "{field}: {error}"
        );
    }
}
