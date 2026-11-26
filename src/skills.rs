//! Agent Skills served over MCP (the `io.modelcontextprotocol/skills`
//! extension, SEP-2640).
//!
//! The skills are embedded in the binary. Each skill is a directory under
//! `skills/` with a `SKILL.md` and optional supporting files, exposed as
//! `skill://<name>/<file>` resources.

use crate::outline::parse_yaml;
use rmcp::{
    ErrorData,
    model::{
        Resource, ResourceContents, Skill, SkillFrontmatter, SkillResource, SkillResources,
        skill_name_from_uri,
    },
};
use serde_json::Value;

const SCHEME: &str = "skill://";
const MIME_MARKDOWN: &str = "text/markdown";

struct SkillFile {
    /// Path relative to the skill directory.
    path: &'static str,
    content: &'static str,
}

struct EmbeddedSkill {
    name: &'static str,
    files: &'static [SkillFile],
}

static SKILLS: &[EmbeddedSkill] = &[EmbeddedSkill {
    name: "processing-markdown",
    files: &[
        SkillFile {
            path: "SKILL.md",
            content: include_str!("../skills/processing-markdown/SKILL.md"),
        },
        SkillFile {
            path: "EXAMPLES.md",
            content: include_str!("../skills/processing-markdown/EXAMPLES.md"),
        },
    ],
}];

fn file_uri(skill: &EmbeddedSkill, file: &SkillFile) -> String {
    format!("{SCHEME}{}/{}", skill.name, file.path)
}

/// Splits the YAML frontmatter of a `SKILL.md` into its fields.
fn parse_frontmatter(content: &str) -> Option<SkillFrontmatter> {
    let rest = content
        .strip_prefix("---")?
        .trim_start_matches(['\r', '\n']);
    let end = rest.find("\n---")?;
    let Value::Object(mut fields) = parse_yaml(&rest[..end]).ok()? else {
        return None;
    };
    let name = fields.remove("name")?.as_str()?.to_owned();
    let description = fields.remove("description")?.as_str()?.to_owned();
    let mut frontmatter = SkillFrontmatter::new(name, description);
    for (key, value) in fields {
        frontmatter = frontmatter.with_field(key, value);
    }
    Some(frontmatter)
}

fn to_skill(skill: &EmbeddedSkill) -> Option<Skill> {
    let entry = skill.files.iter().find(|f| f.path == "SKILL.md")?;
    let frontmatter = parse_frontmatter(entry.content)?;
    let resources = skill
        .files
        .iter()
        .map(|f| SkillResource::from_bytes(file_uri(skill, f), f.content.as_bytes()))
        .collect();
    Some(Skill::new(
        file_uri(skill, entry),
        frontmatter,
        SkillResources::Files(resources),
    ))
}

/// Every skill the server serves.
pub fn list() -> Vec<Skill> {
    SKILLS.iter().filter_map(to_skill).collect()
}

/// The skill whose `SKILL.md` lives at `uri`.
pub fn get(uri: &str) -> Result<Skill, ErrorData> {
    skill_name_from_uri(uri)
        .and_then(|name| SKILLS.iter().find(|s| s.name == name))
        .and_then(to_skill)
        .filter(|skill| skill.uri == uri)
        .ok_or_else(|| ErrorData::invalid_params(format!("unknown skill: {uri}"), None))
}

/// Every skill file as a resource, for `resources/list`.
pub fn resources() -> Vec<Resource> {
    SKILLS
        .iter()
        .flat_map(|skill| {
            skill.files.iter().map(|file| {
                Resource::new(
                    file_uri(skill, file),
                    format!("{}/{}", skill.name, file.path),
                )
                .with_mime_type(MIME_MARKDOWN)
                .with_size(file.content.len() as u64)
            })
        })
        .collect()
}

/// The contents of a skill file, for `resources/read`.
pub fn read(uri: &str) -> Result<ResourceContents, ErrorData> {
    SKILLS
        .iter()
        .flat_map(|skill| skill.files.iter().map(move |file| (skill, file)))
        .find(|(skill, file)| file_uri(skill, file) == uri)
        .map(|(_, file)| ResourceContents::text(file.content, uri).with_mime_type(MIME_MARKDOWN))
        .ok_or_else(|| ErrorData::invalid_params(format!("unknown resource: {uri}"), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::validate_skills;

    #[test]
    fn test_skills_are_valid_and_match_their_manifest() {
        let skills = list();
        assert_eq!(skills.len(), SKILLS.len());
        validate_skills(&skills).unwrap();

        for skill in &skills {
            let SkillResources::Files(files) = &skill.resources else {
                panic!("embedded skills have a full manifest");
            };
            for file in files {
                let ResourceContents::TextResourceContents { text, .. } = read(&file.uri).unwrap()
                else {
                    panic!("skill files are text");
                };
                file.verify(text.as_bytes()).unwrap();
            }
        }
    }

    #[test]
    fn test_get_and_read_reject_unknown_uris() {
        assert!(get("skill://processing-markdown/SKILL.md").is_ok());
        assert!(get("skill://nope/SKILL.md").is_err());
        assert!(get("skill://processing-markdown/EXAMPLES.md").is_err());
        assert!(read("skill://processing-markdown/nope.md").is_err());
    }

    #[test]
    fn test_resources_cover_every_file() {
        let uris: Vec<_> = resources().into_iter().map(|r| r.uri).collect();
        assert!(uris.contains(&"skill://processing-markdown/SKILL.md".to_string()));
        assert!(uris.contains(&"skill://processing-markdown/EXAMPLES.md".to_string()));
    }
}
