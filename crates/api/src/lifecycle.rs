#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Start;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Pause;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Resume;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Shutdown;

#[cfg(all(test, feature = "serde"))]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::RawVmmOp;

    #[test]
    fn lifecycle_operations_have_unit_payloads() {
        for (name, command) in [
            ("Start", Start.into_vmm_command()),
            ("Pause", Pause.into_vmm_command()),
            ("Resume", Resume.into_vmm_command()),
            ("Shutdown", Shutdown.into_vmm_command()),
        ] {
            let op = crate::metadata::OPERATIONS
                .iter()
                .find(|op| op.name == name)
                .unwrap();
            let expected = json!({ name.to_lowercase(): null });
            assert_eq!(serde_json::to_value(command).unwrap(), expected);
            let parsed = (op.parse)(json!(null).into()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn lifecycle_schemas_have_no_object_fields() {
        for schema in [
            schemars::schema_for!(Start),
            schemars::schema_for!(Pause),
            schemars::schema_for!(Resume),
            schemars::schema_for!(Shutdown),
        ] {
            assert_eq!(schema.to_value()["type"], "null");
        }
    }
}
