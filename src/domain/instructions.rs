//! The versions of the instructions a worker reads (goal 113): the hash of
//! the content of the runtime's worker prompt before the task's values go
//! in, of the plugin its session loads and of the repository's instruction
//! documents at the run's base, each recorded on `run_claimed` under its
//! own key. Each is a value of the content alone (never a commit ID or a
//! time), so runs that read the same instructions share it; one that cannot
//! be found is [`UNKNOWN`] and the claim goes on.

use super::Provider;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The value of a hash that could not be found.
pub const UNKNOWN: &str = "unknown";

/// The plugin hash of a worker given no plugin (a Codex worker, or a Claude
/// Code with no dagq plugin installed).
pub const NO_PLUGIN: &str = "none";

/// The repository's instruction documents a worker reads, by path from the
/// repository root: a file, or a directory and everything under it.
pub const REPOSITORY_INSTRUCTIONS: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "docs/development"];

/// The key of the claim's attributes that holds each provider's
/// [`InstructionVersions`] until the claim knows the run's provider
/// ([`settle`]); `run_claimed` never keeps it.
pub const BY_PROVIDER: &str = "instructions_by_provider";

/// Hex characters a hash keeps: enough to tell the versions apart, short
/// enough to read in a table.
const HASH_CHARS: usize = 16;

/// What `run_claimed` records of the instructions a worker read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstructionVersions {
    /// [`template_hash`] of the worker's prompt on the run's provider.
    pub instructions_prompt: String,
    /// [`content_hash`] of the plugin directory the session loads, or
    /// [`NO_PLUGIN`].
    pub instructions_plugin: String,
    /// [`content_hash`] of [`REPOSITORY_INSTRUCTIONS`] at the run's base.
    pub instructions_repo: String,
}

impl InstructionVersions {
    /// Every hash [`UNKNOWN`].
    pub fn unknown() -> Self {
        Self {
            instructions_prompt: UNKNOWN.to_owned(),
            instructions_plugin: UNKNOWN.to_owned(),
            instructions_repo: UNKNOWN.to_owned(),
        }
    }
}

/// Whether `path` (from the repository root) is one of
/// [`REPOSITORY_INSTRUCTIONS`] or under one of its directories.
pub fn is_repository_instruction(path: &str) -> bool {
    REPOSITORY_INSTRUCTIONS.iter().any(|named| {
        path == *named
            || path
                .strip_prefix(named)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The hash of a set of files, each a relative path and its content (for
/// the repository, its blob ID): the same set in any order gives the same
/// value, and a changed byte, path, added or removed file another.
pub fn content_hash<P: AsRef<str>, C: AsRef<[u8]>>(
    files: impl IntoIterator<Item = (P, C)>,
) -> String {
    let mut files: Vec<(P, C)> = files.into_iter().collect();
    files.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
    let mut hasher = Sha256::new();
    for (path, content) in &files {
        let (path, content) = (path.as_ref().as_bytes(), content.as_ref());
        // Lengths first, so no two sets run together into the same bytes.
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path);
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(content);
    }
    short(hasher.finalize().as_slice())
}

/// The hash of a prompt's template: its text with placeholders where the
/// task's values go.
pub fn template_hash(template: &str) -> String {
    short(Sha256::digest(template.as_bytes()).as_slice())
}

fn short(digest: &[u8]) -> String {
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..HASH_CHARS].to_owned()
}

/// The directory of the plugin `name` that Claude Code's
/// `plugins/installed_plugins.json` (`listed`) installs for sessions in
/// `project`: the entry of `<name>@<marketplace>` whose `projectPath` is
/// `project`, else one without a `projectPath` (user scope). An entry is an
/// object or, in the file's second version, a list of them. `None` when
/// none is installed.
pub fn installed_plugin_dir(listed: &Value, name: &str, project: &str) -> Option<String> {
    let plugins = listed.get("plugins")?.as_object()?;
    let mut entries: Vec<&Value> = Vec::new();
    for (id, value) in plugins {
        if id.split_once('@').map_or(id.as_str(), |(plugin, _)| plugin) != name {
            continue;
        }
        match value {
            Value::Array(list) => entries.extend(list),
            entry => entries.push(entry),
        }
    }
    let path = |entry: &&Value| entry.get("installPath")?.as_str().map(str::to_owned);
    let scoped =
        |entry: &Value, to: Option<&str>| entry.get("projectPath").and_then(Value::as_str) == to;
    entries
        .iter()
        .filter(|entry| scoped(entry, Some(project)))
        .find_map(path)
        .or_else(|| {
            entries
                .iter()
                .filter(|entry| scoped(entry, None))
                .find_map(path)
        })
}

/// The versions of the instructions a worker on each of `providers` reads:
/// the hash of its prompt's `template` (none when it cannot be built), the
/// plugin (`claude_plugin` for Claude; a Codex worker is given none, its
/// instructions are the worktree's `AGENTS.md` and its prompt) and
/// [`REPOSITORY_INSTRUCTIONS`] among `repo_blobs`, the blob IDs of the
/// run's base (none when Git could not list them). What cannot be found is
/// [`UNKNOWN`].
pub fn by_provider(
    providers: impl IntoIterator<Item = Provider>,
    template: impl Fn(Provider) -> Option<String>,
    claude_plugin: &str,
    repo_blobs: Option<Vec<(String, String)>>,
) -> BTreeMap<String, InstructionVersions> {
    let repo = repo_blobs.map_or_else(
        || UNKNOWN.to_owned(),
        |blobs| {
            content_hash(
                blobs
                    .into_iter()
                    .filter(|(path, _)| is_repository_instruction(path)),
            )
        },
    );
    let mut versions = BTreeMap::new();
    for provider in providers {
        versions
            .entry(provider.as_str().to_owned())
            .or_insert_with(|| InstructionVersions {
                instructions_prompt: template(provider)
                    .map_or_else(|| UNKNOWN.to_owned(), |text| template_hash(&text)),
                instructions_plugin: match provider {
                    Provider::Claude => claude_plugin.to_owned(),
                    Provider::Codex => NO_PLUGIN.to_owned(),
                },
                instructions_repo: repo.clone(),
            });
    }
    versions
}

/// Put the versions of `provider`'s instructions from [`BY_PROVIDER`] at
/// the top of a claim's attributes, and drop the others; a provider without
/// an entry gets [`InstructionVersions::unknown`]. Attributes without
/// [`BY_PROVIDER`] are left as they are.
pub fn settle(attributes: &mut Map<String, Value>, provider: &str) {
    let Some(mut by_provider) = attributes.remove(BY_PROVIDER) else {
        return;
    };
    let versions = by_provider
        .get_mut(provider)
        .map(Value::take)
        .filter(Value::is_object)
        .unwrap_or_else(|| {
            serde_json::to_value(InstructionVersions::unknown()).unwrap_or(Value::Null)
        });
    if let Value::Object(versions) = versions {
        attributes.extend(versions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_set_of_files_hashes_by_its_content_in_any_order() {
        let files = [("a/SKILL.md", "skill"), ("b.md", "text")];
        let hash = content_hash(files);
        assert_eq!(hash.len(), HASH_CHARS);
        assert_eq!(content_hash(files), hash);
        // The order the files were read in does not count.
        assert_eq!(content_hash([files[1], files[0]]), hash);
        // One byte of one file does.
        assert_ne!(content_hash([("a/SKILL.md", "skilL"), files[1]]), hash);
        // So do a path, an added and a removed file.
        assert_ne!(content_hash([("a/skill.md", "skill"), files[1]]), hash);
        assert_ne!(
            content_hash([files[0], files[1], ("c.md", "")]),
            hash,
            "an added file"
        );
        assert_ne!(content_hash([files[0]]), hash, "a removed file");
        // Paths and contents do not run together.
        assert_ne!(content_hash([("ab", "c")]), content_hash([("a", "bc")]));
        assert_eq!(content_hash(Vec::<(&str, &str)>::new()).len(), HASH_CHARS);
    }

    #[test]
    fn a_template_hashes_by_its_text() {
        let hash = template_hash("Task title: <title>\n");
        assert_eq!(template_hash("Task title: <title>\n"), hash);
        assert_ne!(template_hash("Task title: <title>.\n"), hash);
    }

    #[test]
    fn the_repository_instructions_are_the_named_files_and_directories() {
        for path in [
            "AGENTS.md",
            "CLAUDE.md",
            "docs/development/testing.md",
            "docs/development/sub/x.md",
        ] {
            assert!(is_repository_instruction(path), "{path}");
        }
        for path in [
            "docs/design/prompt.md",
            "docs/development-notes.md",
            "src/AGENTS.md",
            "AGENTS.md.bak",
        ] {
            assert!(!is_repository_instruction(path), "{path}");
        }
    }

    #[test]
    fn the_plugin_dir_is_the_projects_install_else_the_users() {
        let listed = json!({"version": 2, "plugins": {
            "claude-dagq@dagq": [
                {"scope": "project", "projectPath": "/other", "installPath": "/cache/other"},
                {"scope": "user", "installPath": "/cache/user"},
                {"scope": "project", "projectPath": "/repo", "installPath": "/cache/repo"}
            ],
            "claude-dagq-extra@dagq": [{"scope": "user", "installPath": "/cache/extra"}]
        }});
        assert_eq!(
            installed_plugin_dir(&listed, "claude-dagq", "/repo").as_deref(),
            Some("/cache/repo")
        );
        assert_eq!(
            installed_plugin_dir(&listed, "claude-dagq", "/elsewhere").as_deref(),
            Some("/cache/user")
        );
        // The first version kept one object per plugin.
        let first =
            json!({"version": 1, "plugins": {"claude-dagq@dagq": {"installPath": "/cache/v1"}}});
        assert_eq!(
            installed_plugin_dir(&first, "claude-dagq", "/repo").as_deref(),
            Some("/cache/v1")
        );
        for none in [
            json!({"version": 2, "plugins": {}}),
            json!({}),
            json!({"plugins": {"claude-dagq@dagq": [{"scope": "user"}]}}),
        ] {
            assert_eq!(installed_plugin_dir(&none, "claude-dagq", "/repo"), None);
        }
    }

    /// The keys are their own: no axis of `kpi --by` (none is added for
    /// them) and none of the other fields `run_claimed` carries.
    #[test]
    fn the_keys_are_no_axis_and_no_other_claim_field() {
        use crate::domain::kpi::Axis;
        use crate::domain::measure::{ClaimAttributes, ClaimSpacing, HostVersions};
        let keys: Vec<String> = serde_json::to_value(InstructionVersions::unknown())
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            [
                "instructions_plugin",
                "instructions_prompt",
                "instructions_repo"
            ]
        );
        assert_eq!(Axis::ALL.len(), 15);
        let attributes = serde_json::to_value(ClaimAttributes {
            dagq_version: String::new(),
            host: HostVersions {
                claude_version: Some(String::new()),
                codex_version: Some(String::new()),
                rustc_release: Some(String::new()),
                rustc_host: Some(String::new()),
            },
            parallel: 1,
            slots: 0,
            load_avg: None,
            spacing: Some(ClaimSpacing {
                claim_spacing: 1,
                claim_spacing_wait_secs: 0,
            }),
            light_room: Some(true),
            instructions: Default::default(),
            candidates: None,
            by_task: Default::default(),
        })
        .unwrap();
        // What the claim itself adds beside the attributes.
        let claim = [
            "from",
            "to",
            "provider",
            "requested_provider",
            "worker_mode",
            "provider_version",
            "model",
            "effort",
            "group",
            "ladder_model",
            "model_unknown",
            "trial_percentile",
            "escalation_inherited",
        ];
        for key in &keys {
            assert!(Axis::ALL.iter().all(|axis| axis.as_str() != key), "{key}");
            assert!(attributes.get(key).is_none(), "{key}");
            assert!(!claim.contains(&key.as_str()), "{key}");
        }
    }

    /// Each provider has its own template and plugin, all share the
    /// repository's documents, and what cannot be found is unknown.
    #[test]
    fn each_provider_gets_its_versions_or_unknown() {
        let blobs = vec![
            ("AGENTS.md".to_owned(), "a1".to_owned()),
            ("src/main.rs".to_owned(), "b2".to_owned()),
        ];
        let template = |provider: Provider| Some(format!("template {}", provider.as_str()));
        let versions = by_provider(
            [Provider::Claude, Provider::Codex, Provider::Claude],
            template,
            "plugin",
            Some(blobs),
        );
        assert_eq!(versions.len(), 2);
        let (claude, codex) = (&versions["claude"], &versions["codex"]);
        assert_eq!(claude.instructions_prompt, template_hash("template claude"));
        assert_eq!(codex.instructions_prompt, template_hash("template codex"));
        assert_eq!(claude.instructions_plugin, "plugin");
        assert_eq!(codex.instructions_plugin, NO_PLUGIN);
        // Only the instruction documents count.
        assert_eq!(
            claude.instructions_repo,
            content_hash([("AGENTS.md", "a1")])
        );
        assert_eq!(codex.instructions_repo, claude.instructions_repo);

        let unknown = by_provider([Provider::Claude], |_| None, UNKNOWN, None);
        assert_eq!(unknown["claude"], InstructionVersions::unknown());
        assert!(by_provider([], template, "plugin", None).is_empty());
    }

    #[test]
    fn settling_keeps_the_runs_providers_versions_or_unknown() {
        let versions = |tag: &str| {
            serde_json::to_value(InstructionVersions {
                instructions_prompt: format!("prompt-{tag}"),
                instructions_plugin: format!("plugin-{tag}"),
                instructions_repo: "repo".to_owned(),
            })
            .unwrap()
        };
        let attributes = json!({
            "parallel": 2,
            BY_PROVIDER: {"claude": versions("claude"), "codex": versions("codex")},
        });
        let mut codex = attributes.as_object().unwrap().clone();
        settle(&mut codex, "codex");
        assert_eq!(
            Value::Object(codex),
            json!({
                "parallel": 2,
                "instructions_prompt": "prompt-codex",
                "instructions_plugin": "plugin-codex",
                "instructions_repo": "repo",
            })
        );
        // A provider the supervisor found nothing for is unknown, and the
        // claim goes on with the rest.
        let mut other = json!({"parallel": 2, BY_PROVIDER: {"claude": versions("claude")}})
            .as_object()
            .unwrap()
            .clone();
        settle(&mut other, "codex");
        assert_eq!(other["instructions_prompt"], UNKNOWN);
        assert_eq!(other["instructions_plugin"], UNKNOWN);
        assert_eq!(other["instructions_repo"], UNKNOWN);
        assert_eq!(other["parallel"], 2);
        assert!(other.get(BY_PROVIDER).is_none());
        // Attributes without them stay as they are.
        let mut plain = json!({"parallel": 2}).as_object().unwrap().clone();
        settle(&mut plain, "claude");
        assert_eq!(Value::Object(plain), json!({"parallel": 2}));
    }
}
