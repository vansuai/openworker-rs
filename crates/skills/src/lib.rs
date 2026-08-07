//! Skills system — SKILL.md format with progressive disclosure.

pub mod skill;
pub mod store;

pub use skill::{
    effective_skills, loaded_skill_names, skill_catalog_text, LoadSkillTool, Skill, SkillLoader,
};
pub use store::{
    validate_name, SessionSkillStore, SkillCreated, SkillMoved, SkillRow, SkillStore, SkillUpdated,
    SkillUploadPreview, GLOBAL_SCOPE, PROJECT_SCOPE,
};
