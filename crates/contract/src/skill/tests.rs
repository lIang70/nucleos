#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn a_skill_root_is_a_name_not_a_path() {
    let root = SkillRootName::new("engineering").unwrap();
    assert_eq!(root.as_str(), "engineering");
    assert_eq!(serde_json::to_string(&root).unwrap(), "\"engineering\"");
    assert!(SkillRootName::new("/srv/skills").is_err());
    assert!(SkillRootName::new("two words").is_err());
    assert!(serde_json::from_str::<SkillRootName>("\"/srv/skills\"").is_err());
}
