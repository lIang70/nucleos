#![allow(clippy::unwrap_used)]

use super::*;
use crate::content::ToolName;

#[test]
fn a_tool_name_is_shared_and_validated() {
    let name = ToolName::new("files.read").unwrap();
    let definition = ToolDefinition {
        name: name.clone(),
        description: "Read a file".to_owned(),
        input_schema: serde_json::json!({"type": "object"}),
    };
    assert!(definition.validate().is_ok());
    assert_eq!(name.as_str(), "files.read");
    assert!(ToolName::new("").is_err());
    assert!(ToolName::new("two words").is_err());
    assert!(serde_json::from_str::<ToolName>("\"bad name\"").is_err());
}
