//! the agent policy function: design.md §10 (principals and policy) and §11 (operations table).
//! contract: `decide` answers allow/deny and merge class for one operation on one document;
//! `allowed_ops` lists every operation an actor may currently perform on a candidate document,
//! for the context packet (design.md §6).
//!
//! this is the coarse gate: access, layer ceiling, status/agent_policy, budget, provenance, then
//! a class estimate from the design.md §11 table. the fine-grained structure check (critical
//! sections byte-identical, banners, required headings) is `check::gate::classify_change`, which
//! runs again at merge time against the base commit and is the source of truth for those rules.

use crate::config::Config;
use crate::model::{Access, AgentPolicy, Frontmatter, Layer, MergeClass, Operation, Status};
use serde::Serialize;
use std::fmt;

/// who is acting: a human by principal name, or an agent by skill and principal (a service
/// account in groups.toml).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorKind {
    Human,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub kind: ActorKind,
    pub principal: String,
    pub skill: Option<String>,
}

impl Actor {
    /// parse `human:<principal>` or `agent:<skill>@<principal>`; the skill may be omitted
    /// (`agent:<principal>`).
    pub fn parse(s: &str) -> Result<Actor, String> {
        let (kind_str, rest) = s.split_once(':').ok_or_else(|| {
            format!("actor `{s}` must look like `human:<principal>` or `agent:<skill>@<principal>`")
        })?;
        let kind = match kind_str {
            "human" => ActorKind::Human,
            "agent" => ActorKind::Agent,
            other => return Err(format!("unknown actor kind `{other}`, expected `human` or `agent`")),
        };
        let (skill, principal) = match rest.split_once('@') {
            Some((skill, principal)) => (Some(skill.to_string()), principal.to_string()),
            None => (None, rest.to_string()),
        };
        if principal.is_empty() {
            return Err(format!("actor `{s}` is missing a principal"));
        }
        Ok(Actor { kind, principal, skill })
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.kind, &self.skill) {
            (ActorKind::Human, _) => write!(f, "human:{}", self.principal),
            (ActorKind::Agent, Some(skill)) => write!(f, "agent:{skill}@{}", self.principal),
            (ActorKind::Agent, None) => write!(f, "agent:{}", self.principal),
        }
    }
}

/// the policy-relevant slice of one document, read fresh at decision time.
#[derive(Debug, Clone, PartialEq)]
pub struct DocPolicyView {
    pub id: String,
    pub layer: Layer,
    pub status: Status,
    pub agent_policy: AgentPolicy,
    pub owner: String,
    pub access: Access,
    /// criticality band; `None` below the minimum corpus (design.md §17.4).
    pub band: Option<MergeClass>,
    pub word_count: usize,
    pub in_canon: u32,
    pub in_code: u32,
    pub has_source_ref: bool,
}

impl DocPolicyView {
    /// build from a parsed document's frontmatter. `band`, `in_canon`, and `in_code` come from
    /// the index; a caller without one (this crate's `cli::policy`) passes `None`/`0`.
    pub fn from_frontmatter(
        cfg: &Config,
        fm: &Frontmatter,
        word_count: usize,
        band: Option<MergeClass>,
        in_canon: u32,
        in_code: u32,
    ) -> anyhow::Result<DocPolicyView> {
        let missing = fm.missing_required();
        if !missing.is_empty() {
            anyhow::bail!("frontmatter is missing required field(s): {}", missing.join(", "));
        }
        let layer = fm.layer.expect("checked by missing_required");
        Ok(DocPolicyView {
            id: fm.id.clone().expect("checked by missing_required"),
            layer,
            status: fm.status.expect("checked by missing_required"),
            agent_policy: fm.agent_policy.unwrap_or_else(|| cfg.agent_policy_for(layer)),
            owner: fm.owner.clone().expect("checked by missing_required"),
            access: effective_access(cfg, &fm.access),
            band,
            word_count,
            in_canon,
            in_code,
            has_source_ref: fm.refs.iter().any(|r| r.rel == "source"),
        })
    }
}

/// `access.read` defaults to `["*"]` and `access.write` to `["owner"]` plus kb.toml's
/// `[defaults] write_groups`, when a document omits `[access]` entirely (design.md §6, §10).
fn effective_access(cfg: &Config, fm_access: &Option<Access>) -> Access {
    match fm_access {
        Some(a) => a.clone(),
        None => {
            let mut write = vec!["owner".to_string()];
            for g in &cfg.kb.defaults.write_groups {
                if !write.contains(g) {
                    write.push(g.clone());
                }
            }
            Access { read: vec!["*".to_string()], write }
        }
    }
}

/// one requested operation. `target_layer` names the layer a `create` would land on when no
/// drafted document exists yet to read a layer from; ignored for every other operation.
#[derive(Debug, Clone)]
pub struct OpRequest {
    pub op: Operation,
    pub section: Option<String>,
    pub field: Option<String>,
    pub added_words: usize,
    pub target_layer: Option<Layer>,
}

/// the policy function's verdict: allow or deny, the merge class the structure gate would likely
/// assign, why, and (only on a budget denial) which operation to suggest instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub allow: bool,
    pub class: MergeClass,
    pub reasons: Vec<String>,
    pub suggest: Option<Operation>,
}

/// one entry of `allowed_ops`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllowedOp {
    pub op: Operation,
    pub class: MergeClass,
    pub reasons: Vec<String>,
}

/// decide whether `actor` may perform `req` on `doc`, and at what merge class.
/// order (design.md §10): write access, layer ceiling, operation permitted for status and
/// agent_policy, budget and provenance guards, then class by the design.md §11 table.
/// humans are never denied; they are classified only (the structure gate does the real
/// classification for a human diff, at merge time).
pub fn decide(cfg: &Config, actor: &Actor, doc: Option<&DocPolicyView>, req: &OpRequest) -> Decision {
    if actor.kind == ActorKind::Human {
        return decide_human(cfg, doc, req);
    }

    // the layer this operation targets: the document's own layer, or `target_layer` for a
    // hypothetical `create` with no drafted document yet, or the repo default.
    let layer = doc.map(|d| d.layer).or(req.target_layer).unwrap_or(cfg.kb.defaults.layer);

    // 1. write access. a hypothetical create with no drafted document has no access list yet to
    // check; the ceiling and the owner assigned at creation time govern it instead.
    if let Some(d) = doc {
        let groups = cfg.groups_of(&actor.principal);
        let single_user = cfg.groups.principals.is_empty();
        if !has_write_access(&actor.principal, &groups, d) && !single_user {
            return deny("access: principal is not in access.write, is not the owner, and no group it belongs to is listed there");
        }
    }
    // an empty groups.toml is a single-user repository: the one person running agents here is
    // every document's writer and their agents may work up to `procedure` (design.md §10).
    // listing any principal ends this mode.
    let single_user = cfg.groups.principals.is_empty();
    let single_user_note = single_user.then(|| "single-user mode: groups.toml lists no principals; write access granted and ceiling is procedure".to_string());

    // 2. layer ceiling. a service principal with no `layer_ceiling` configured may only touch
    // `reference` documents.
    let ceiling = cfg
        .groups
        .principals
        .get(&actor.principal)
        .and_then(|p| p.layer_ceiling)
        .unwrap_or(if single_user { Layer::Procedure } else { Layer::Reference });
    if layer.rank() < ceiling.rank() {
        return deny(&format!(
            "ceiling: {layer} (rank {}) is above the principal's ceiling {ceiling} (rank {})",
            layer.rank(),
            ceiling.rank()
        ));
    }

    // 3. operation permitted for status and agent_policy. a hypothetical create with no drafted
    // document has no status to check yet; the ceiling above already bounds it.
    if let Some(d) = doc
        && let Err(reason) = op_permitted(d.status, d.agent_policy, req.op)
    {
        return deny(&format!(
            "{reason}: `{}` is not permitted for status {} and agent_policy {}",
            req.op, d.status, d.agent_policy
        ));
    }

    // 4. guards: budget, then provenance.
    let word_count = doc.map(|d| d.word_count).unwrap_or(0) + req.added_words;
    let budget = cfg.budget_for(layer);
    if word_count > budget {
        let mut decision = deny(&format!("budget: {word_count} words exceeds the {layer} budget of {budget}"));
        decision.suggest = Some(Operation::Escalate);
        return decision;
    }
    let has_source_ref = doc.map(|d| d.has_source_ref).unwrap_or(false);
    if req.op == Operation::Create && !has_source_ref {
        return deny("provenance: create requires a source ref");
    }

    // 5. class, by the design.md §11 table.
    let mut decision = classify(cfg, doc, req, layer);
    if let Some(note) = single_user_note {
        decision.reasons.push(note);
    }
    decision
}

fn decide_human(cfg: &Config, doc: Option<&DocPolicyView>, req: &OpRequest) -> Decision {
    let layer = doc.map(|d| d.layer).or(req.target_layer).unwrap_or(cfg.kb.defaults.layer);
    let above_band = doc.is_some_and(|d| d.band == Some(MergeClass::Review));
    let class = if matches!(layer, Layer::Principle | Layer::Design) || above_band {
        MergeClass::Review
    } else {
        MergeClass::Auto
    };
    Decision { allow: true, class, reasons: vec!["human: classified, never denied".to_string()], suggest: None }
}

fn deny(reason: &str) -> Decision {
    Decision { allow: false, class: MergeClass::Review, reasons: vec![reason.to_string()], suggest: None }
}

fn has_write_access(principal: &str, groups: &[String], doc: &DocPolicyView) -> bool {
    let write = &doc.access.write;
    write.iter().any(|w| w == "*")
        || write.iter().any(|w| w == principal)
        || (write.iter().any(|w| w == "owner") && principal == doc.owner)
        || write.iter().any(|w| groups.contains(w))
}

/// which operations design.md §11 permits at all for a document's status and agent_policy,
/// independent of criticality or band; those only affect merge class, not whether the operation
/// runs. `Err` names which rule blocked it: `"lifecycle"` for `delete`'s own precondition,
/// `"agent policy"` otherwise.
fn op_permitted(status: Status, policy: AgentPolicy, op: Operation) -> Result<(), &'static str> {
    if op == Operation::Delete {
        return if matches!(status, Status::Superseded | Status::Archived) {
            Ok(())
        } else {
            // a draft past its ttl also qualifies per design.md §11; this function has no age
            // input, so only the status half of that rule is checked here (see this crate's
            // report for the gap).
            Err("lifecycle")
        };
    }
    if matches!(status, Status::Superseded | Status::Archived) {
        return if matches!(op, Operation::Link | Operation::Escalate) { Ok(()) } else { Err("agent policy") };
    }
    match policy {
        // locked does not block an operation here: every agent operation on a locked document is
        // still review class, decided in `classify`, not denied.
        AgentPolicy::Locked | AgentPolicy::Editable => Ok(()),
        AgentPolicy::AppendOnly => {
            if matches!(
                op,
                Operation::Append
                    | Operation::Link
                    | Operation::Escalate
                    | Operation::Supersede
                    | Operation::Retire
                    | Operation::Create
            ) {
                Ok(())
            } else {
                Err("agent policy")
            }
        }
    }
}

fn classify(cfg: &Config, doc: Option<&DocPolicyView>, req: &OpRequest, layer: Layer) -> Decision {
    let agent_policy = doc.map(|d| d.agent_policy).unwrap_or_else(|| cfg.agent_policy_for(layer));
    let in_canon = doc.map(|d| d.in_canon).unwrap_or(0);
    let in_code = doc.map(|d| d.in_code).unwrap_or(0);
    let band = doc.and_then(|d| d.band);

    let (in_band, mut reasons) = match band {
        Some(MergeClass::Review) => (false, Vec::new()),
        Some(MergeClass::Auto) => (true, Vec::new()),
        None => (true, vec!["band undefined: corpus below minimum".to_string()]),
    };

    let editable = agent_policy == AgentPolicy::Editable;
    let append_capable = matches!(agent_policy, AgentPolicy::Editable | AgentPolicy::AppendOnly);

    let mut class = match req.op {
        Operation::Create => {
            if matches!(layer, Layer::Reference | Layer::Procedure) { MergeClass::Auto } else { MergeClass::Review }
        }
        Operation::Append => {
            if append_capable && in_band { MergeClass::Auto } else { MergeClass::Review }
        }
        Operation::Revise => {
            if editable && in_band && !is_critical_section(cfg, layer, req.section.as_deref()) {
                MergeClass::Auto
            } else {
                MergeClass::Review
            }
        }
        Operation::Rewrite => {
            reasons.push(
                "rewrite: byte-identity of every critical section is checked by the structure gate at merge time"
                    .to_string(),
            );
            if editable && in_band { MergeClass::Auto } else { MergeClass::Review }
        }
        Operation::Supersede => {
            if layer.rank() <= Layer::Decision.rank() {
                MergeClass::Review
            } else if editable && in_band && in_canon == 0 {
                MergeClass::Auto
            } else {
                MergeClass::Review
            }
        }
        Operation::Split | Operation::Merge | Operation::Delete => MergeClass::Review,
        Operation::Link => {
            if req.field.as_deref() == Some("remove") {
                if editable && in_band && !is_critical_section(cfg, layer, req.section.as_deref()) {
                    MergeClass::Auto
                } else {
                    MergeClass::Review
                }
            } else if append_capable {
                MergeClass::Auto
            } else {
                MergeClass::Review
            }
        }
        Operation::Retire => {
            if in_band && in_canon == 0 && in_code == 0 { MergeClass::Auto } else { MergeClass::Review }
        }
        Operation::Escalate => MergeClass::Review,
    };

    if agent_policy == AgentPolicy::Locked {
        class = MergeClass::Review;
        reasons.push("agent_policy is locked: every agent operation is review class".to_string());
    }

    Decision { allow: true, class, reasons, suggest: None }
}

/// true when `section`'s last slug segment is declared critical for `layer` (or the layer
/// declares every section critical). this reads only the layer default from manifest.toml; a
/// per-document `[structure]` override is not visible here (`DocPolicyView` carries none) and is
/// enforced by the structure gate instead, which does see it.
fn is_critical_section(cfg: &Config, layer: Layer, section: Option<&str>) -> bool {
    let structure = cfg.structure_for(layer);
    if structure.critical.iter().any(|c| c == "*") {
        return true;
    }
    match section {
        Some(s) => {
            let last = s.rsplit('/').next().unwrap_or(s);
            structure.critical.iter().any(|c| c == last)
        }
        None => false,
    }
}

const ALL_OPS: [Operation; 11] = [
    Operation::Create,
    Operation::Append,
    Operation::Revise,
    Operation::Rewrite,
    Operation::Supersede,
    Operation::Split,
    Operation::Merge,
    Operation::Link,
    Operation::Retire,
    Operation::Delete,
    Operation::Escalate,
];

/// every operation `actor` may currently perform on `doc`, with its class and reasons. this is
/// what the context packet embeds per candidate document (design.md §6). evaluated with a
/// neutral request (no section, no field, no added words), so a `revise` or `link` remove that
/// would only be auto outside a critical section is conservatively reported at its section-less
/// class.
pub fn allowed_ops(cfg: &Config, actor: &Actor, doc: &DocPolicyView) -> Vec<AllowedOp> {
    ALL_OPS
        .iter()
        .filter_map(|&op| {
            let req = OpRequest { op, section: None, field: None, added_words: 0, target_layer: None };
            let decision = decide(cfg, actor, Some(doc), &req);
            decision.allow.then_some(AllowedOp { op, class: decision.class, reasons: decision.reasons })
        })
        .collect()
}

/// the human accountable for a change: the human principal itself, or an agent's owner from
/// groups.toml (falling back to the principal name when groups.toml does not know it).
pub fn responsible_party(cfg: &Config, actor: &Actor) -> String {
    match actor.kind {
        ActorKind::Human => actor.principal.clone(),
        ActorKind::Agent => {
            cfg.groups.principals.get(&actor.principal).and_then(|p| p.owner.clone()).unwrap_or_else(|| actor.principal.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Groups, KbConfig, Manifest, Principal};
    use std::path::PathBuf;

    fn base_cfg() -> Config {
        Config { root: PathBuf::from("."), kb: KbConfig::default(), manifest: Manifest::default(), groups: Groups::default() }
    }

    /// a config with three principals: `svc-curate` (service, ceiling `procedure`, owner
    /// `caleb`, group `agents`), `svc-allarea` (service, ceiling `principle`, so the table test
    /// below can exercise every layer without the ceiling rule interfering), and `caleb` (human).
    fn cfg_with_groups() -> Config {
        let mut c = base_cfg();
        c.groups.principals.insert(
            "svc-curate".to_string(),
            Principal { kind: "service".to_string(), emails: vec![], owner: Some("caleb".to_string()), groups: vec!["agents".to_string()], layer_ceiling: Some(Layer::Procedure) },
        );
        c.groups.principals.insert(
            "svc-allarea".to_string(),
            Principal { kind: "service".to_string(), emails: vec![], owner: Some("caleb".to_string()), groups: vec![], layer_ceiling: Some(Layer::Principle) },
        );
        c.groups.principals.insert(
            "caleb".to_string(),
            Principal { kind: "human".to_string(), emails: vec!["caleb@example.com".to_string()], owner: None, groups: vec!["admins".to_string()], layer_ceiling: None },
        );
        c.groups.groups.insert("agents".to_string(), crate::config::Group { members: vec!["svc-curate".to_string()] });
        c
    }

    fn agent(principal: &str) -> Actor {
        Actor { kind: ActorKind::Agent, principal: principal.to_string(), skill: Some("curate".to_string()) }
    }

    fn human(principal: &str) -> Actor {
        Actor { kind: ActorKind::Human, principal: principal.to_string(), skill: None }
    }

    #[allow(clippy::too_many_arguments)]
    fn view(
        layer: Layer,
        status: Status,
        policy: AgentPolicy,
        owner: &str,
        write: &[&str],
        band: Option<MergeClass>,
        in_canon: u32,
        in_code: u32,
        has_source_ref: bool,
    ) -> DocPolicyView {
        DocPolicyView {
            id: "01K5A7Q2XW3ZRCM8T6YB4VJH0N".to_string(),
            layer,
            status,
            agent_policy: policy,
            owner: owner.to_string(),
            access: Access { read: vec!["*".to_string()], write: write.iter().map(|s| s.to_string()).collect() },
            band,
            word_count: 100,
            in_canon,
            in_code,
            has_source_ref,
        }
    }

    fn op_req(op: Operation) -> OpRequest {
        OpRequest { op, section: None, field: None, added_words: 0, target_layer: None }
    }

    // --- actor parsing ---

    #[test]
    fn actor_parse_and_display_roundtrip() {
        let h = Actor::parse("human:caleb").unwrap();
        assert_eq!(h, Actor { kind: ActorKind::Human, principal: "caleb".to_string(), skill: None });
        assert_eq!(h.to_string(), "human:caleb");

        let a = Actor::parse("agent:curate@svc-curate").unwrap();
        assert_eq!(a, Actor { kind: ActorKind::Agent, principal: "svc-curate".to_string(), skill: Some("curate".to_string()) });
        assert_eq!(a.to_string(), "agent:curate@svc-curate");

        assert!(Actor::parse("robot:x").is_err());
        assert!(Actor::parse("human:").is_err());
        assert!(Actor::parse("no-colon").is_err());
    }

    // --- access ---

    #[test]
    fn access_denied_when_principal_not_in_write_group() {
        let c = cfg_with_groups();
        let d = view(Layer::Reference, Status::Canon, AgentPolicy::Editable, "caleb", &["someone-else"], Some(MergeClass::Auto), 0, 0, true);
        let decision = decide(&c, &agent("svc-curate"), Some(&d), &op_req(Operation::Append));
        assert!(!decision.allow);
        assert!(decision.reasons[0].starts_with("access"), "{:?}", decision.reasons);
    }

    #[test]
    fn owner_keyword_grants_write_access() {
        let c = cfg_with_groups();
        let d = view(Layer::Reference, Status::Canon, AgentPolicy::Editable, "svc-curate", &["owner"], Some(MergeClass::Auto), 0, 0, true);
        let decision = decide(&c, &agent("svc-curate"), Some(&d), &op_req(Operation::Append));
        assert!(decision.allow, "{:?}", decision.reasons);
    }

    #[test]
    fn group_membership_grants_write_access() {
        let c = cfg_with_groups();
        let d = view(Layer::Reference, Status::Canon, AgentPolicy::Editable, "caleb", &["agents"], Some(MergeClass::Auto), 0, 0, true);
        let decision = decide(&c, &agent("svc-curate"), Some(&d), &op_req(Operation::Append));
        assert!(decision.allow, "{:?}", decision.reasons);
    }

    // --- ceiling ---

    #[test]
    fn ceiling_denies_a_design_create_for_a_procedure_ceiling() {
        let c = cfg_with_groups();
        let req = OpRequest { op: Operation::Create, section: None, field: None, added_words: 0, target_layer: Some(Layer::Design) };
        let decision = decide(&c, &agent("svc-curate"), None, &req);
        assert!(!decision.allow);
        assert!(decision.reasons[0].starts_with("ceiling"), "{:?}", decision.reasons);
    }

    // --- guards ---

    #[test]
    fn budget_denial_suggests_escalate() {
        let c = cfg_with_groups();
        let budget = c.budget_for(Layer::Reference);
        let d = view(Layer::Reference, Status::Canon, AgentPolicy::Editable, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        let mut req = op_req(Operation::Append);
        req.added_words = budget + 1; // word_count 100 + this pushes well over budget
        let decision = decide(&c, &agent("svc-curate"), Some(&d), &req);
        assert!(!decision.allow);
        assert!(decision.reasons[0].starts_with("budget"), "{:?}", decision.reasons);
        assert_eq!(decision.suggest, Some(Operation::Escalate));
    }

    #[test]
    fn create_without_source_ref_is_denied_for_provenance() {
        let c = cfg_with_groups();
        let req = OpRequest { op: Operation::Create, section: None, field: None, added_words: 0, target_layer: Some(Layer::Reference) };
        let decision = decide(&c, &agent("svc-curate"), None, &req);
        assert!(!decision.allow);
        assert!(decision.reasons[0].starts_with("provenance"), "{:?}", decision.reasons);
    }

    // --- status / agent_policy gates ---

    #[test]
    fn locked_agent_policy_forces_review_but_does_not_deny() {
        let c = cfg_with_groups();
        let d = view(Layer::Reference, Status::Canon, AgentPolicy::Locked, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        let decision = decide(&c, &agent("svc-curate"), Some(&d), &op_req(Operation::Append));
        assert!(decision.allow);
        assert_eq!(decision.class, MergeClass::Review);
        assert!(decision.reasons.iter().any(|r| r.contains("locked")));
    }

    #[test]
    fn append_only_permits_append_but_not_revise() {
        let c = cfg_with_groups();
        // procedure, not decision, so the principal's `procedure` ceiling does not itself deny
        // the operation and the test isolates the append-only rule.
        let d = view(Layer::Procedure, Status::Canon, AgentPolicy::AppendOnly, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        let append = decide(&c, &agent("svc-curate"), Some(&d), &op_req(Operation::Append));
        assert!(append.allow);
        assert_eq!(append.class, MergeClass::Auto);

        let revise = decide(&c, &agent("svc-curate"), Some(&d), &op_req(Operation::Revise));
        assert!(!revise.allow);
        assert!(revise.reasons[0].starts_with("agent policy"), "{:?}", revise.reasons);
    }

    #[test]
    fn superseded_document_permits_only_link_delete_escalate() {
        let c = cfg_with_groups();
        let d = view(Layer::Reference, Status::Superseded, AgentPolicy::Editable, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        for op in [Operation::Link, Operation::Delete, Operation::Escalate] {
            let decision = decide(&c, &agent("svc-curate"), Some(&d), &op_req(op));
            assert!(decision.allow, "{op} should be allowed on a superseded document: {:?}", decision.reasons);
        }
        for op in [Operation::Append, Operation::Revise, Operation::Rewrite, Operation::Supersede, Operation::Retire, Operation::Create] {
            let decision = decide(&c, &agent("svc-curate"), Some(&d), &op_req(op));
            assert!(!decision.allow, "{op} should be denied on a superseded document");
            assert!(decision.reasons[0].starts_with("agent policy"), "{op}: {:?}", decision.reasons);
        }
    }

    #[test]
    fn delete_requires_superseded_or_archived_status() {
        let c = cfg_with_groups();
        let canon = view(Layer::Reference, Status::Canon, AgentPolicy::Editable, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        let denied = decide(&c, &agent("svc-curate"), Some(&canon), &op_req(Operation::Delete));
        assert!(!denied.allow);
        assert!(denied.reasons[0].starts_with("lifecycle"), "{:?}", denied.reasons);

        let archived = view(Layer::Reference, Status::Archived, AgentPolicy::Editable, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        let allowed = decide(&c, &agent("svc-curate"), Some(&archived), &op_req(Operation::Delete));
        assert!(allowed.allow);
        assert_eq!(allowed.class, MergeClass::Review);
    }

    // --- humans ---

    #[test]
    fn human_is_always_allowed_and_only_classified() {
        let c = cfg_with_groups();
        // a locked design document would deny every agent operation's frontmatter change; a
        // human is still allowed, just classified.
        let d = view(Layer::Design, Status::Canon, AgentPolicy::Locked, "someone-else", &["nobody"], Some(MergeClass::Review), 9, 9, false);
        for op in ALL_OPS {
            let decision = decide(&c, &human("caleb"), Some(&d), &op_req(op));
            assert!(decision.allow, "{op} should always be allowed for a human");
        }
        let d2 = view(Layer::Reference, Status::Canon, AgentPolicy::Editable, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        let decision = decide(&c, &human("caleb"), Some(&d2), &op_req(Operation::Revise));
        assert_eq!(decision.class, MergeClass::Auto);
        assert_eq!(decision.reasons, vec!["human: classified, never denied".to_string()]);
    }

    // --- allowed_ops ---

    #[test]
    fn allowed_ops_for_an_editable_reference_in_band() {
        let c = cfg_with_groups();
        let d = view(Layer::Reference, Status::Canon, AgentPolicy::Editable, "caleb", &["*"], Some(MergeClass::Auto), 0, 0, true);
        let ops = allowed_ops(&c, &agent("svc-curate"), &d);
        let find = |op: Operation| ops.iter().find(|a| a.op == op);

        assert_eq!(find(Operation::Append).map(|a| a.class), Some(MergeClass::Auto));
        assert_eq!(find(Operation::Retire).map(|a| a.class), Some(MergeClass::Auto));
        assert_eq!(find(Operation::Split).map(|a| a.class), Some(MergeClass::Review));
        // delete needs superseded/archived status or a stale draft; canon has neither.
        assert!(find(Operation::Delete).is_none(), "{ops:?}");
    }

    #[test]
    fn allowed_ops_for_a_locked_design() {
        let c = cfg_with_groups();
        let d = view(Layer::Design, Status::Canon, AgentPolicy::Locked, "caleb", &["*"], Some(MergeClass::Auto), 3, 1, true);
        let ops = allowed_ops(&c, &agent("svc-allarea"), &d);
        assert!(!ops.is_empty());
        for a in &ops {
            assert_eq!(a.class, MergeClass::Review, "{:?} should be review class under locked", a.op);
        }
        assert!(ops.iter().all(|a| a.op != Operation::Delete), "{ops:?}");
    }

    // --- responsible_party ---

    #[test]
    fn responsible_party_resolves_agent_owner() {
        let c = cfg_with_groups();
        assert_eq!(responsible_party(&c, &human("caleb")), "caleb");
        assert_eq!(responsible_party(&c, &agent("svc-curate")), "caleb");
        assert_eq!(responsible_party(&c, &agent("svc-unknown")), "svc-unknown");
    }

    // --- table-driven: every row of design.md §11 ---

    enum Expect {
        Allow(MergeClass),
        Deny(&'static str),
    }

    struct Case {
        name: &'static str,
        layer: Layer,
        status: Status,
        policy: AgentPolicy,
        band: Option<MergeClass>,
        in_canon: u32,
        in_code: u32,
        op: Operation,
        section: Option<&'static str>,
        field: Option<&'static str>,
        expect: Expect,
    }

    #[test]
    fn sec11_operations_table() {
        let c = cfg_with_groups();
        let cases = vec![
            Case { name: "create reference is auto", layer: Layer::Reference, status: Status::Draft, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Create, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "create procedure is auto", layer: Layer::Procedure, status: Status::Draft, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Create, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "create decision is review", layer: Layer::Decision, status: Status::Draft, policy: AgentPolicy::AppendOnly, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Create, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "append editable in band is auto", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Append, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "append above band is review", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Review), in_canon: 0, in_code: 0, op: Operation::Append, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "append append-only in band is auto", layer: Layer::Decision, status: Status::Canon, policy: AgentPolicy::AppendOnly, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Append, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "revise critical section is review", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Revise, section: Some("prerequisites"), field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "revise non-critical section is auto", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Revise, section: Some("notes"), field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "revise denied on append-only", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::AppendOnly, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Revise, section: Some("notes"), field: None, expect: Expect::Deny("agent policy") },
            Case { name: "rewrite editable in band is auto", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Rewrite, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "rewrite above band is review", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Review), in_canon: 0, in_code: 0, op: Operation::Rewrite, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "supersede no canon dependents is auto", layer: Layer::Reference, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Supersede, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "supersede with canon dependents is review", layer: Layer::Reference, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 3, in_code: 0, op: Operation::Supersede, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "supersede decision and above is always review", layer: Layer::Decision, status: Status::Canon, policy: AgentPolicy::AppendOnly, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Supersede, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "split is always review", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Split, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "merge is always review", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Merge, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "link add editable is auto", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Link, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "link add append-only is auto", layer: Layer::Decision, status: Status::Canon, policy: AgentPolicy::AppendOnly, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Link, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "link remove non-critical follows revise: auto", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Link, section: Some("notes"), field: Some("remove"), expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "link remove critical follows revise: review", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Link, section: Some("rollback"), field: Some("remove"), expect: Expect::Allow(MergeClass::Review) },
            Case { name: "retire no dependents in band is auto", layer: Layer::Reference, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Retire, section: None, field: None, expect: Expect::Allow(MergeClass::Auto) },
            Case { name: "retire with dependents is review", layer: Layer::Reference, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 1, in_code: 0, op: Operation::Retire, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "delete on canon is denied (lifecycle)", layer: Layer::Reference, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Delete, section: None, field: None, expect: Expect::Deny("lifecycle") },
            Case { name: "delete on superseded is review", layer: Layer::Reference, status: Status::Superseded, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Delete, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
            Case { name: "escalate is always review", layer: Layer::Procedure, status: Status::Canon, policy: AgentPolicy::Editable, band: Some(MergeClass::Auto), in_canon: 0, in_code: 0, op: Operation::Escalate, section: None, field: None, expect: Expect::Allow(MergeClass::Review) },
        ];

        for case in cases {
            let d = view(case.layer, case.status, case.policy, "svc-allarea", &["*"], case.band, case.in_canon, case.in_code, true);
            let req = OpRequest {
                op: case.op,
                section: case.section.map(str::to_string),
                field: case.field.map(str::to_string),
                added_words: 0,
                target_layer: None,
            };
            let decision = decide(&c, &agent("svc-allarea"), Some(&d), &req);
            match case.expect {
                Expect::Allow(class) => {
                    assert!(decision.allow, "{}: expected allow, got deny {:?}", case.name, decision.reasons);
                    assert_eq!(decision.class, class, "{}: {:?}", case.name, decision.reasons);
                }
                Expect::Deny(prefix) => {
                    assert!(!decision.allow, "{}: expected deny, got allow {:?}", case.name, decision.reasons);
                    assert!(decision.reasons[0].starts_with(prefix), "{}: {:?}", case.name, decision.reasons);
                }
            }
        }
    }

    #[test]
    fn single_user_mode_grants_write_and_procedure_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::load(dir.path()).unwrap();
        assert!(cfg.groups.principals.is_empty());
        let actor = Actor::parse("agent:curate@someone@example.com").unwrap();
        let fm = crate::model::Frontmatter {
            id: Some("01K5A7Q2XW3ZRCM8T6YB4VJH0N".into()),
            layer: Some(Layer::Procedure),
            title: Some("rotate tokens".into()),
            status: Some(Status::Canon),
            agent_policy: Some(AgentPolicy::Editable),
            owner: Some("other@example.com".into()),
            ..Default::default()
        };
        let doc = DocPolicyView::from_frontmatter(&cfg, &fm, 100, None, 0, 0).unwrap();
        let d = decide(&cfg, &actor, Some(&doc), &OpRequest { op: Operation::Append, section: None, field: None, added_words: 10, target_layer: None });
        assert!(d.allow, "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.starts_with("single-user mode")));
        let ops = allowed_ops(&cfg, &actor, &doc);
        assert!(ops.iter().any(|o| o.op == Operation::Append));
    }
}
