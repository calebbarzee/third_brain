//! kb.toml, manifest.toml, groups.toml. typed with serde defaults; `Config::load` tolerates missing files.
use crate::model::{AgentPolicy, Layer, Structure};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KbConfig {
    pub store: StoreConfig,
    /// role -> provider. roles: curate, classify, contextualize, answer, review, resolve, embed, rerank.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// layer -> agent_policy override.
    pub layers: BTreeMap<String, AgentPolicy>,
    pub defaults: Defaults,
    pub secrets: SecretsConfig,
    pub code_repos: Vec<CodeRepo>,
    pub sources: toml::Table,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StoreConfig {
    /// local | object
    pub backend: String,
    /// local directory, relative to the repo root
    pub path: String,
    pub bucket: Option<String>,
}
impl Default for StoreConfig {
    fn default() -> Self {
        Self { backend: "local".into(), path: ".kb/store".into(), bucket: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderConfig {
    /// anthropic | openai | ollama
    pub kind: String,
    pub model: String,
    pub base_url: Option<String>,
    /// environment variable holding the api key; never the key itself
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    /// groups added to access.write when a document omits [access]
    pub write_groups: Vec<String>,
    pub layer: Layer,
}
impl Default for Defaults {
    fn default() -> Self {
        Self { write_groups: vec![], layer: Layer::Reference }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SecretsConfig {
    pub rules: Vec<SecretRule>,
    /// paths (glob) exempt from scanning
    pub allow_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretRule {
    pub id: String,
    pub regex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CodeRepo {
    pub name: String,
    pub url: String,
    pub default_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub parser_version: u32,
    pub chunker_version: u32,
    pub embedding_model: String,
    /// layer name -> word budget
    pub budgets: BTreeMap<String, usize>,
    pub thresholds: Thresholds,
    pub weights: Weights,
    /// layer name -> critical/required headings
    pub structure: BTreeMap<String, Structure>,
    pub checksums: BTreeMap<String, String>,
    pub packet: Packet,
}
impl Default for Manifest {
    fn default() -> Self {
        Self {
            parser_version: 1,
            chunker_version: 1,
            embedding_model: String::new(),
            budgets: BTreeMap::new(),
            thresholds: Thresholds::default(),
            weights: Weights::default(),
            structure: default_structure(),
            checksums: BTreeMap::new(),
            packet: Packet::default(),
        }
    }
}

fn default_structure() -> BTreeMap<String, Structure> {
    let mut m = BTreeMap::new();
    m.insert("principle".into(), Structure { critical: vec!["*".into()], required: vec![] });
    m.insert("design".into(), Structure { critical: vec!["*".into()], required: vec![] });
    m.insert("decision".into(), Structure { critical: vec!["decision".into(), "consequences".into()], required: vec![] });
    m.insert("procedure".into(), Structure { critical: vec!["prerequisites".into(), "rollback".into()], required: vec![] });
    m
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    pub criticality_band_pct: f64,
    pub min_corpus: usize,
    pub near_duplicate: f64,
    pub review_confidence: f64,
    pub nightly_revert_confidence: f64,
    pub draft_ttl_days: u32,
    pub superseded_delete_days: u32,
    pub orphan_days: u32,
    pub classify_retries: u32,
    pub review_retries: u32,
    pub session_max_ops: u32,
    pub session_max_minutes: u32,
    pub diff_max_docs: u32,
    pub diff_max_fraction: f64,
    pub merge_retries: u32,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            criticality_band_pct: 20.0,
            min_corpus: 50,
            near_duplicate: 0.92,
            review_confidence: 0.7,
            nightly_revert_confidence: 0.9,
            draft_ttl_days: 30,
            superseded_delete_days: 90,
            orphan_days: 90,
            classify_retries: 3,
            review_retries: 2,
            session_max_ops: 50,
            session_max_minutes: 30,
            diff_max_docs: 20,
            diff_max_fraction: 0.3,
            merge_retries: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Weights {
    pub in_canon: f64,
    pub in_code: f64,
    pub pagerank: f64,
    pub reads_90d: f64,
    pub cites_90d: f64,
    pub pagerank_damping: f64,
}
impl Default for Weights {
    fn default() -> Self {
        Self { in_canon: 0.25, in_code: 0.30, pagerank: 0.20, reads_90d: 0.15, cites_90d: 0.10, pagerank_damping: 0.85 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Packet {
    pub max_tokens: u32,
    pub capture_excerpt_tokens: u32,
    pub review_context_tokens: u32,
}
impl Default for Packet {
    fn default() -> Self {
        Self { max_tokens: 32_000, capture_excerpt_tokens: 4_000, review_context_tokens: 8_000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Groups {
    pub principals: BTreeMap<String, Principal>,
    pub groups: BTreeMap<String, Group>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Principal {
    /// human | service
    pub kind: String,
    pub emails: Vec<String>,
    pub owner: Option<String>,
    pub groups: Vec<String>,
    pub layer_ceiling: Option<Layer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Group {
    pub members: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub kb: KbConfig,
    pub manifest: Manifest,
    pub groups: Groups,
}

impl Config {
    /// walk up from `start` to the first directory holding kb.toml or .git.
    pub fn find_root(start: &Path) -> Option<PathBuf> {
        let mut cur = Some(start.to_path_buf());
        while let Some(dir) = cur {
            if dir.join("kb.toml").is_file() || dir.join(".git").exists() {
                return Some(dir);
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
        None
    }

    /// load all three files; a missing file yields defaults. `.kb/kb.local.toml` overrides kb.toml keys.
    pub fn load(root: &Path) -> Result<Config> {
        let kb: KbConfig = read_or_default(&root.join("kb.toml"))?;
        let kb = match read_toml_table(&root.join(".kb/kb.local.toml"))? {
            Some(local) => {
                let mut base = toml::Value::try_from(&kb)?;
                merge_tables(&mut base, toml::Value::Table(local));
                base.try_into()?
            }
            None => kb,
        };
        Ok(Config {
            root: root.to_path_buf(),
            kb,
            manifest: read_or_default(&root.join("manifest.toml"))?,
            groups: read_or_default(&root.join("groups.toml"))?,
        })
    }

    pub fn agent_policy_for(&self, layer: Layer) -> AgentPolicy {
        self.kb.layers.get(layer.as_str()).copied().unwrap_or_else(|| layer.default_agent_policy())
    }

    pub fn budget_for(&self, layer: Layer) -> usize {
        self.manifest.budgets.get(layer.as_str()).copied().unwrap_or_else(|| layer.default_budget_words())
    }

    pub fn structure_for(&self, layer: Layer) -> Structure {
        self.manifest.structure.get(layer.as_str()).cloned().unwrap_or_default()
    }

    /// principal name for a git author email, if groups.toml knows it.
    pub fn principal_for_email(&self, email: &str) -> Option<String> {
        self.groups
            .principals
            .iter()
            .find(|(_, p)| p.emails.iter().any(|e| e.eq_ignore_ascii_case(email)))
            .map(|(name, _)| name.clone())
    }

    /// groups a principal belongs to, from both the principal row and group member lists.
    pub fn groups_of(&self, principal: &str) -> Vec<String> {
        let mut out: Vec<String> = self.groups.principals.get(principal).map(|p| p.groups.clone()).unwrap_or_default();
        for (g, grp) in &self.groups.groups {
            if grp.members.iter().any(|m| m == principal) && !out.contains(g) {
                out.push(g.clone());
            }
        }
        out.sort();
        out
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".kb")
    }
}

fn read_or_default<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    if !path.is_file() {
        return Ok(T::default());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn read_toml_table(path: &Path) -> Result<Option<toml::Table>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(Some(toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?))
}

fn merge_tables(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) if existing.is_table() && v.is_table() => merge_tables(existing, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load(dir.path()).unwrap();
        assert_eq!(c.agent_policy_for(Layer::Decision), AgentPolicy::AppendOnly);
        assert_eq!(c.budget_for(Layer::Reference), 6_000);
        assert_eq!(c.manifest.thresholds.min_corpus, 50);
        assert_eq!(c.structure_for(Layer::Decision).critical, vec!["decision", "consequences"]);
    }

    #[test]
    fn overrides_and_groups() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kb.toml"), "[layers]\ndecision = \"editable\"\n[providers.classify]\nkind = \"ollama\"\nmodel = \"llama3\"\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".kb")).unwrap();
        std::fs::write(dir.path().join(".kb/kb.local.toml"), "[providers.classify]\nmodel = \"qwen\"\n").unwrap();
        std::fs::write(dir.path().join("manifest.toml"), "[budgets]\nreference = 100\n").unwrap();
        std::fs::write(dir.path().join("groups.toml"), "[principals.caleb]\nkind = \"human\"\nemails = [\"c@x.com\"]\ngroups = [\"admins\"]\n[groups.platform]\nmembers = [\"caleb\"]\n").unwrap();
        let c = Config::load(dir.path()).unwrap();
        assert_eq!(c.agent_policy_for(Layer::Decision), AgentPolicy::Editable);
        assert_eq!(c.kb.providers["classify"].model, "qwen");
        assert_eq!(c.kb.providers["classify"].kind, "ollama");
        assert_eq!(c.budget_for(Layer::Reference), 100);
        assert_eq!(c.principal_for_email("C@x.com").as_deref(), Some("caleb"));
        assert_eq!(c.groups_of("caleb"), vec!["admins", "platform"]);
    }
}
