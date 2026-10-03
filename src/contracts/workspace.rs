use super::*;

/// Whether `kind` belongs to the RPC family the canonical join pairs
/// (PRD-CTR-REQ-024): gRPC today; Thrift and tRPC flip this arm when added.
pub fn is_rpc_family(kind: ContractKind) -> bool {
    matches!(kind, ContractKind::Grpc)
}

/// Normalize a workspace identifier for comparison: trim surrounding
/// whitespace and case-fold (PRD-CTR-REQ-019).
pub fn normalize_workspace_id(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// One workspace's contracts offered to the canonical join.
///
/// `workspace` is the declared identifier (pre-normalized — the join folds
/// it itself); the join never fabricates one (PRD-CTR-REQ-015's repo-name
/// defaulting happens at scope construction, TASK-084).
pub struct RpcJoinScope<'a> {
    /// Workspace identifier as declared.
    pub workspace: String,
    /// Contract candidates of that workspace (mixed kinds/roles allowed).
    pub candidates: &'a [ContractCandidate],
}

/// Which tolerance recovered a relaxed pair (PRD-CTR-REQ-024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcMatchBasis {
    /// Service names differ only by package qualification
    /// (`users.v1.UserService` vs `UserService` — compared on the last
    /// dot-segment, case-folded).
    PackageQualifiedService,
    /// Method names differ only by casing (`get_user` vs `GetUser`).
    CaseFoldedMethod,
    /// The provider registered the whole service (`*` identifier) and
    /// pairs with any method-level consumer of that service.
    ServiceLevelProvider,
}

/// One side of a relaxed link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcJoinSide {
    /// Canonical ID of the matched candidate (developer spelling).
    pub canonical_id: String,
    /// Workspace the candidate lives in (as declared).
    pub workspace: String,
    /// Provider or consumer.
    pub role: ContractRole,
}

/// A provider↔consumer pair recovered by the second matching pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcJoin {
    /// The serving side.
    pub provider: RpcJoinSide,
    /// The calling side.
    pub consumer: RpcJoinSide,
    /// Why the pair matched despite unequal canonical IDs.
    pub basis: RpcMatchBasis,
}

/// Run the canonical join over workspace-scoped candidate slices.
///
/// Deterministic: consumers are visited in input order (scope order, then
/// candidate order); the best provider is method-level before service-level,
/// then the lowest (scope, candidate) index. Pure — no storage, no mutation.
#[derive(Clone, Copy)]
struct CandidateRef {
    scope: usize,
    candidate: usize,
}
struct MatchedRefs {
    provider: CandidateRef,
    consumer: CandidateRef,
    basis: RpcMatchBasis,
}

pub fn canonical_rpc_join(scopes: &[RpcJoinScope]) -> Vec<RpcJoin> {
    rpc_join_refs(scopes)
        .0
        .into_iter()
        .map(|pair| {
            let side = |r: CandidateRef, role| RpcJoinSide {
                canonical_id: scopes[r.scope].candidates[r.candidate].canonical_id.clone(),
                workspace: scopes[r.scope].workspace.clone(),
                role,
            };
            RpcJoin {
                provider: side(pair.provider, ContractRole::Provider),
                consumer: side(pair.consumer, ContractRole::Consumer),
                basis: pair.basis,
            }
        })
        .collect()
}

fn rpc_join_refs(scopes: &[RpcJoinScope]) -> (Vec<MatchedRefs>, usize) {
    use std::collections::{HashMap, HashSet};
    let workspaces: Vec<_> = scopes
        .iter()
        .map(|s| normalize_workspace_id(&s.workspace))
        .collect();
    let mut provider_ids = HashSet::new();
    let mut consumer_ids = HashSet::new();
    let mut work = 0;
    for (si, scope) in scopes.iter().enumerate() {
        for cand in scope.candidates.iter().filter(|c| is_rpc_family(c.kind)) {
            let set = if cand.role == ContractRole::Provider {
                &mut provider_ids
            } else {
                &mut consumer_ids
            };
            set.insert((workspaces[si].clone(), cand.canonical_id.clone()));
            work += 1;
        }
    }
    let mut methods = HashMap::new();
    let mut wildcards = HashMap::new();
    let mut consumers = Vec::new();
    for (si, scope) in scopes.iter().enumerate() {
        for (ci, cand) in scope
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| is_rpc_family(c.kind))
        {
            let counterpart = if cand.role == ContractRole::Provider {
                &consumer_ids
            } else {
                &provider_ids
            };
            work += 1;
            if counterpart.contains(&(workspaces[si].clone(), cand.canonical_id.clone())) {
                continue;
            }
            let Some(service) = last_segment_folded(&cand.qualifier) else {
                continue;
            };
            let candidate = CandidateRef {
                scope: si,
                candidate: ci,
            };
            if cand.role == ContractRole::Consumer {
                consumers.push((candidate, service, rpc_method_key(&cand.identifier)));
            } else if cand.identifier == "*" {
                wildcards
                    .entry((workspaces[si].clone(), service))
                    .or_insert(candidate);
            } else {
                methods
                    .entry((
                        workspaces[si].clone(),
                        service,
                        rpc_method_key(&cand.identifier),
                    ))
                    .or_insert(candidate);
            }
        }
    }
    let mut joins = Vec::new();
    for (consumer, service, method) in consumers {
        let workspace = &workspaces[consumer.scope];
        work += 1;
        let (provider, wildcard) =
            if let Some(p) = methods.get(&(workspace.clone(), service.clone(), method)) {
                (*p, false)
            } else {
                work += 1;
                let Some(p) = wildcards.get(&(workspace.clone(), service)) else {
                    continue;
                };
                (*p, true)
            };
        let p = &scopes[provider.scope].candidates[provider.candidate];
        let c = &scopes[consumer.scope].candidates[consumer.candidate];
        let basis = if wildcard {
            RpcMatchBasis::ServiceLevelProvider
        } else if p.qualifier != c.qualifier {
            RpcMatchBasis::PackageQualifiedService
        } else {
            RpcMatchBasis::CaseFoldedMethod
        };
        joins.push(MatchedRefs {
            provider,
            consumer,
            basis,
        });
    }
    (joins, work)
}

/// Relaxed match of one consumer against one provider (both RPC family):
/// service compared on the last dot-segment case-folded, method case-folded,
/// `*` = service-level registration. IDs are never rewritten here — only
/// compared tolerantly.
#[cfg(test)]
pub(super) fn rpc_relaxed_match(
    consumer: &ContractCandidate,
    provider: &ContractCandidate,
) -> Option<RpcMatchBasis> {
    let consumer_service = last_segment_folded(&consumer.qualifier)?;
    let provider_service = last_segment_folded(&provider.qualifier)?;
    if consumer_service != provider_service {
        return None;
    }
    if provider.identifier == "*" {
        return Some(RpcMatchBasis::ServiceLevelProvider);
    }
    if rpc_method_key(&provider.identifier) == rpc_method_key(&consumer.identifier) {
        return Some(if provider.qualifier != consumer.qualifier {
            RpcMatchBasis::PackageQualifiedService
        } else {
            RpcMatchBasis::CaseFoldedMethod
        });
    }
    None
}

/// Match key for an RPC method name: case-folded with word separators
/// (`_`) removed, so the proto spelling, camelCase stubs, and tonic's
/// snake_case impls of one method compare equal
/// (`get_user` = `GetUser` = `getUser`).
pub(super) fn rpc_method_key(method: &str) -> String {
    method
        .chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Last dot-segment of a service qualifier, case-folded for comparison.
pub(super) fn last_segment_folded(qualifier: &str) -> Option<String> {
    let last = qualifier.trim().rsplit('.').next()?.trim().to_lowercase();
    if last.is_empty() { None } else { Some(last) }
}

// ---------------------------------------------------------------------------
// Storage query API (TASK-083)
// ---------------------------------------------------------------------------

/// Filters for [`list_contracts`] (`wonk contracts` CLI, PRD-CTR-REQ-008).
///
/// `None` kind/role means no filter on that axis; `orphans` restricts the
/// result to consumers with no provider for the same canonical ID.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContractQuery {
    /// Restrict to one contract kind.
    pub kind: Option<ContractKind>,
    /// Restrict to one role.
    pub role: Option<ContractRole>,
    /// Only orphan consumers (no same-canonical_id provider in this repo).
    pub orphans: bool,
}

/// One stored contract row with the owning symbol's name resolved.
///
/// `symbol` is `None` for file-level contracts (documents, top-level
/// registrations) whose `symbol_id` is NULL.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractRow {
    /// `<kind>::<qualifier>::<identifier>` canonical ID.
    pub canonical_id: String,
    /// Contract kind.
    pub kind: ContractKind,
    /// Provider or consumer.
    pub role: ContractRole,
    /// Owning symbol name, when `symbol_id` resolved at write time.
    pub symbol: Option<String>,
    /// Path relative to repo root.
    pub file: String,
    /// 1-based line of the detection site.
    pub line: usize,
    /// 1.0 framework-recognized / 0.5 heuristic (AR-018).
    pub confidence: f64,
}

/// List stored contracts matching `query`.
///
/// One static statement — NULL parameters disable their filter, so no SQL
/// is ever assembled dynamically. Reads only the passed connection: a
/// single-repo index answers from its own rows and never errors for lack
/// of sibling repos (PRD-CTR-REQ-012, the degenerate case of REQ-006's
/// workspace scoping that TASK-084 widens).
pub fn list_contracts(
    conn: &rusqlite::Connection,
    query: &ContractQuery,
) -> anyhow::Result<Vec<ContractRow>> {
    let mut stmt = conn.prepare(
        "SELECT c.canonical_id, c.kind, c.role, s.name, c.file, c.line, c.confidence \
         FROM contracts c LEFT JOIN symbols s ON s.id = c.symbol_id \
         WHERE (?1 IS NULL OR c.kind = ?1) \
           AND (?2 IS NULL OR c.role = ?2) \
           AND (?3 = 0 OR (c.role = 'consumer' AND NOT EXISTS ( \
                SELECT 1 FROM contracts p \
                WHERE p.canonical_id = c.canonical_id AND p.role = 'provider'))) \
         ORDER BY c.kind, c.canonical_id, c.file, c.line",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            query.kind.map(|k| k.as_str()),
            query.role.map(|r| r.as_str()),
            i64::from(query.orphans),
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, f64>(6)?,
            ))
        },
    )?;

    let mut out = Vec::new();
    for row in rows {
        let (canonical_id, kind, role, symbol, file, line, confidence) = row?;
        let kind = kind
            .parse()
            .map_err(|e| anyhow::anyhow!("corrupt contract row {canonical_id}: {e}"))?;
        let role = role
            .parse()
            .map_err(|e| anyhow::anyhow!("corrupt contract row {canonical_id}: {e}"))?;
        out.push(ContractRow {
            canonical_id,
            kind,
            role,
            symbol,
            file,
            line: line.max(0) as usize,
            confidence,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Cross-repo resolution (TASK-084)
// ---------------------------------------------------------------------------

/// A repo's workspace membership, with the REQ-015 default materialized
/// once at construction so the matcher never carries an `if unset` branch.
///
/// `declared` is the verbatim repo-local config value; `effective` is the
/// normalized set the matcher compares against (trimmed, case-folded) —
/// the repo's own name when nothing is declared.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceScope {
    /// Last path component of the repo root.
    pub repo_name: String,
    /// Verbatim declared workspace ids (may be empty).
    pub declared: Vec<String>,
    /// Normalized effective set: declared when present, own name otherwise.
    pub effective: Vec<String>,
}

/// Resolve a repo's workspace scope (PRD-CTR-REQ-015, AR-025).
pub fn workspace_scope(declared: &[String], repo_root: &std::path::Path) -> WorkspaceScope {
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let effective = if declared.is_empty() {
        vec![normalize_workspace_id(&repo_name)]
    } else {
        declared.iter().map(|w| normalize_workspace_id(w)).collect()
    };
    WorkspaceScope {
        repo_name,
        declared: declared.to_vec(),
        effective,
    }
}

/// One same-workspace repo found in the central registry (DR-031: only
/// `meta.json` is read at discovery; the index opens lazily, DR-030).
#[derive(Debug, Clone)]
pub struct SiblingRepo {
    /// Display name: basename, disambiguated by canonical root when necessary.
    pub name: String,
    /// Absolute path to the sibling's repository root.
    /// Absolute path to the sibling's `index.db`.
    pub index_path: std::path::PathBuf,
    /// Normalized effective workspaces (stored declared set, or the
    /// sibling's own name when it stored nothing).
    pub workspaces: Vec<String>,
}

/// Scan `repos_dir` for same-workspace repos, skipping `own_root`.
///
/// Discovery reads only each entry's `meta.json` — a sibling's working-tree
/// config is never opened (PRD-CTR-REQ-014/020). The shared walk and
/// validation live in [`crate::db::registry_entries`]; this layers the
/// own-repo skip and the workspace intersection on top, then sorts by name
/// (deterministic). Never opens any `index.db`.
pub fn scan_registry(
    repos_dir: &std::path::Path,
    own_root: &std::path::Path,
    own_effective: &[String],
) -> Vec<SiblingRepo> {
    registry_members(repos_dir, own_root, own_effective)
        .1
        .into_iter()
        .map(|(member, _)| member)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RepoId(std::path::PathBuf);

impl RepoId {
    fn new(root: &std::path::Path) -> Self {
        Self(std::fs::canonicalize(root).unwrap_or_else(|_| {
            let absolute = if root.is_absolute() {
                root.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(root)
            };
            let mut normalized = std::path::PathBuf::new();
            for component in absolute.components() {
                match component {
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        normalized.pop();
                    }
                    other => normalized.push(other.as_os_str()),
                }
            }
            normalized
        }))
    }
    fn label(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

fn registry_members(
    repos_dir: &std::path::Path,
    own_root: &std::path::Path,
    effective: &[String],
) -> (String, Vec<(SiblingRepo, RepoId)>) {
    use std::collections::HashSet;
    let own_id = RepoId::new(own_root);
    let own_name = workspace_scope(&[], own_root).repo_name;
    let mut names = HashMap::from([(own_name.clone(), 1usize)]);
    let mut seen = HashSet::from([own_id.clone()]);
    let mut members = Vec::new();
    for (root, index_path, meta) in crate::db::registry_entries(repos_dir) {
        let id = RepoId::new(&root);
        let scope = workspace_scope(&meta.workspaces, &root);
        if !scope.effective.iter().any(|w| effective.contains(w)) || !seen.insert(id.clone()) {
            continue;
        }
        *names.entry(scope.repo_name.clone()).or_default() += 1;
        members.push((
            SiblingRepo {
                name: scope.repo_name,
                index_path,
                workspaces: scope.effective,
            },
            id,
        ));
    }
    for (member, id) in &mut members {
        if names[&member.name] > 1 {
            member.name = id.label();
        }
    }
    members.sort_by(|a, b| a.0.name.cmp(&b.0.name).then_with(|| a.1.0.cmp(&b.1.0)));
    let own_label = if names[&own_name] > 1 {
        own_id.label()
    } else {
        own_name
    };
    (own_label, members)
}

/// Lazy connections over the scanned member set (DR-030): an index is
/// opened with `db::open_existing` on first use and cached — non-members
/// are never opened at all, and members cost one open per resolution.
/// The cache itself is the shared [`crate::db::ConnectionCache`].
pub struct SiblingConnections {
    open: crate::db::ConnectionCache,
}

impl SiblingConnections {
    pub(super) fn new() -> Self {
        Self {
            open: crate::db::ConnectionCache::new(),
        }
    }

    /// Get or lazily open the sibling's index connection.
    pub(super) fn connection(
        &mut self,
        sibling: &SiblingRepo,
    ) -> anyhow::Result<&rusqlite::Connection> {
        self.open.get_or_open(&sibling.index_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to open sibling index {}: {e}",
                sibling.index_path.display()
            )
        })
    }
}

/// The registry directory the CLI and MCP resolve links against:
/// `$HOME/.wonk/repos`. `None` when no home directory exists.
pub(crate) fn default_repos_dir() -> Option<std::path::PathBuf> {
    crate::config::home_dir().map(|h| h.join(".wonk").join("repos"))
}

/// Why a resolved provider↔consumer pair matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkBasis {
    /// Canonical IDs are equal (first pass, REQ-005).
    ExactId,
    /// IDs differ; the RPC-relaxed join recovered the pair (REQ-024).
    Rpc(RpcMatchBasis),
}

impl std::fmt::Display for LinkBasis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkBasis::ExactId => write!(f, "exact"),
            LinkBasis::Rpc(RpcMatchBasis::PackageQualifiedService) => {
                write!(f, "rpc:package-qualified-service")
            }
            LinkBasis::Rpc(RpcMatchBasis::CaseFoldedMethod) => write!(f, "rpc:case-folded-method"),
            LinkBasis::Rpc(RpcMatchBasis::ServiceLevelProvider) => {
                write!(f, "rpc:service-level-provider")
            }
        }
    }
}

/// One endpoint (side) of a cross-repo link.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkEndpoint {
    /// Display name: basename, or canonical root when workspace basenames collide.
    pub repo: String,
    /// `<kind>::<qualifier>::<identifier>` canonical ID.
    pub canonical_id: String,
    /// Contract kind.
    pub kind: ContractKind,
    /// Path relative to that repo's root.
    pub file: String,
    /// 1-based line of the detection site.
    pub line: usize,
    /// Owning symbol name, when stored.
    pub symbol: Option<String>,
    /// Stored confidence.
    pub confidence: f64,
}

impl LinkEndpoint {
    fn from_row(repo: &str, row: &ContractRow) -> Self {
        Self {
            repo: repo.to_string(),
            canonical_id: row.canonical_id.clone(),
            kind: row.kind,
            file: row.file.clone(),
            line: row.line,
            symbol: row.symbol.clone(),
            confidence: row.confidence,
        }
    }
}

/// A provider↔consumer pair resolved across repo boundaries.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossRepoLink {
    /// Why the pair matched.
    pub basis: LinkBasis,
    /// The serving side.
    pub provider: LinkEndpoint,
    /// The calling side.
    pub consumer: LinkEndpoint,
}

/// Every own-repo consumer row carries exactly one of these (AR-025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsumerStatus {
    /// An opposite-role match (exact or RPC-relaxed) exists in this repo
    /// or any same-workspace sibling.
    Linked,
    /// No match anywhere in the workspace AND this repo declares ≥1
    /// workspace.
    Orphan,
    /// No match AND nothing declared — the effective workspace is the
    /// repo's own name. A configuration gap is never labeled a defect.
    Unscoped,
}

impl ConsumerStatus {
    /// Grep-token form used by `wonk contracts` rows.
    pub fn as_str(self) -> &'static str {
        match self {
            ConsumerStatus::Linked => "linked",
            ConsumerStatus::Orphan => "orphan",
            ConsumerStatus::Unscoped => "unscoped",
        }
    }
}

/// Live (never persisted, DR-031) result of resolving one repo's workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceResolution {
    /// The querying repo's workspace scope.
    pub scope: WorkspaceScope,
    /// Same-workspace siblings found in the registry.
    pub siblings: Vec<SiblingRepo>,
    /// Resolved cross-repo pairs involving this repo.
    pub links: Vec<CrossRepoLink>,
    /// Status for every own consumer row, keyed by
    /// `(canonical_id, file, line)`.
    pub status: HashMap<(String, String, usize), ConsumerStatus>,
    /// Own provider rows with no consumer in this repo or any sibling.
    pub unused_providers: Vec<ContractRow>,
}

/// Row identity used for status keys and link dedup.
pub(super) fn row_key(row: &ContractRow) -> (String, String, usize) {
    (row.canonical_id.clone(), row.file.clone(), row.line)
}

/// Rebuild a comparable candidate from a stored row (split the canonical
/// ID back into qualifier + identifier). `None` for malformed IDs.
pub fn row_to_candidate(row: &ContractRow) -> Option<ContractCandidate> {
    let mut parts = row.canonical_id.splitn(3, "::");
    let kind = parts.next()?.parse::<ContractKind>().ok()?;
    let qualifier = parts.next()?.to_string();
    let identifier = parts.next()?.to_string();
    if identifier.is_empty() {
        return None;
    }
    Some(ContractCandidate {
        kind,
        role: row.role,
        qualifier,
        identifier,
        canonical_id: row.canonical_id.clone(),
        params: Vec::new(),
        owning_symbol: row.symbol.clone(),
        line: row.line,
        confidence: row.confidence,
    })
}

/// Per-repo contract rows indexed for opposite-role lookup.
pub(super) struct RepoRows {
    id: RepoId,
    name: String,
    providers_by_id: HashMap<String, Vec<ContractRow>>,
    consumers_by_id: HashMap<String, Vec<ContractRow>>,
}

impl RepoRows {
    fn build(id: RepoId, name: &str, rows: &[ContractRow]) -> Self {
        let mut providers_by_id: HashMap<String, Vec<ContractRow>> = HashMap::new();
        let mut consumers_by_id: HashMap<String, Vec<ContractRow>> = HashMap::new();
        for row in rows {
            let slot = if row.role == ContractRole::Provider {
                &mut providers_by_id
            } else {
                &mut consumers_by_id
            };
            slot.entry(row.canonical_id.clone())
                .or_default()
                .push(row.clone());
        }
        Self {
            id,
            name: name.to_string(),
            providers_by_id,
            consumers_by_id,
        }
    }
}

/// Resolve provider↔consumer links across the same-workspace registry
/// (TASK-084). Pure query-time joins over existing indexes — nothing is
/// written anywhere (DR-031). With no members this is a no-op pass over
/// `own_rows` (PRD-CTR-REQ-012).
///
/// Pass 1 (exact): own consumers × member providers and own providers ×
/// member consumers by equal canonical ID. In-repo pairs participate only
/// as status, never as links. Pass 2 (RPC-relaxed): for each shared
/// workspace, own + member gRPC candidates go through
/// [`canonical_rpc_join`], whose built-in exact exclusion keeps the first
/// pass authoritative.
pub fn resolve_workspace(
    own_root: &std::path::Path,
    own_rows: &[ContractRow],
    declared: &[String],
    repos_dir: &std::path::Path,
) -> anyhow::Result<WorkspaceResolution> {
    let mut scope = workspace_scope(declared, own_root);
    let (own_label, identified_members) = registry_members(repos_dir, own_root, &scope.effective);
    scope.repo_name = own_label;
    let siblings: Vec<_> = identified_members.iter().map(|(s, _)| s.clone()).collect();
    let mut conns = SiblingConnections::new();

    let own = RepoRows::build(RepoId::new(own_root), &scope.repo_name, own_rows);
    let mut members = Vec::new();
    for (sibling, id) in &identified_members {
        let conn = conns.connection(sibling)?;
        let rows = list_contracts(conn, &ContractQuery::default())?;
        members.push(RepoRows::build(id.clone(), &sibling.name, &rows));
    }

    let mut links: Vec<CrossRepoLink> = Vec::new();
    let mut linked_consumer_keys: std::collections::HashSet<(String, String, usize)> =
        std::collections::HashSet::new();
    let mut consumed_provider_keys: std::collections::HashSet<(String, String, usize)> =
        std::collections::HashSet::new();

    // Exact pass: own consumers served by member providers.
    for (id, crows) in &own.consumers_by_id {
        for m in &members {
            let Some(prows) = m.providers_by_id.get(id) else {
                continue;
            };
            for c in crows {
                linked_consumer_keys.insert(row_key(c));
                for p in prows {
                    links.push(CrossRepoLink {
                        basis: LinkBasis::ExactId,
                        provider: LinkEndpoint::from_row(&m.name, p),
                        consumer: LinkEndpoint::from_row(&own.name, c),
                    });
                }
            }
        }
    }
    // Exact pass: own providers consumed by member consumers.
    for (id, prows) in &own.providers_by_id {
        for m in &members {
            let Some(crows) = m.consumers_by_id.get(id) else {
                continue;
            };
            for p in prows {
                consumed_provider_keys.insert(row_key(p));
                for c in crows {
                    links.push(CrossRepoLink {
                        basis: LinkBasis::ExactId,
                        provider: LinkEndpoint::from_row(&own.name, p),
                        consumer: LinkEndpoint::from_row(&m.name, c),
                    });
                }
            }
        }
    }

    // Retain concrete origins through matching; IDs alone cannot recover a chosen repo.
    let mut shared = scope.effective.clone();
    shared.sort();
    shared.dedup();
    let own_rpc: Vec<_> = own_rows
        .iter()
        .filter(|r| is_rpc_family(r.kind))
        .cloned()
        .collect();
    let member_rpc: Vec<_> = members.iter().map(RepoRows::grpc_rows).collect();
    let mut pairs_seen = std::collections::HashSet::new();
    for workspace in &shared {
        let mut sets = Vec::new();
        let mut add = |repo: &RepoRows, rows: &[ContractRow]| {
            let pairs: Vec<_> = rows
                .iter()
                .filter_map(|r| row_to_candidate(r).map(|c| (r.clone(), c)))
                .collect();
            if !pairs.is_empty() {
                sets.push((repo.id.clone(), repo.name.clone(), pairs));
            }
        };
        add(&own, &own_rpc);
        for ((sibling, member), rows) in siblings.iter().zip(&members).zip(&member_rpc) {
            if sibling.workspaces.contains(workspace) {
                add(member, rows);
            }
        }
        let candidates: Vec<Vec<_>> = sets
            .iter()
            .map(|(_, _, rows)| rows.iter().map(|(_, c)| c.clone()).collect())
            .collect();
        let scopes: Vec<_> = candidates
            .iter()
            .map(|c| RpcJoinScope {
                workspace: workspace.clone(),
                candidates: c,
            })
            .collect();
        for pair in rpc_join_refs(&scopes).0 {
            let (provider_id, provider_name, provider_rows) = &sets[pair.provider.scope];
            let (consumer_id, consumer_name, consumer_rows) = &sets[pair.consumer.scope];
            let provider = &provider_rows[pair.provider.candidate].0;
            let consumer = &consumer_rows[pair.consumer.candidate].0;
            if provider_id == &own.id {
                consumed_provider_keys.insert(row_key(provider));
            }
            if consumer_id == &own.id {
                linked_consumer_keys.insert(row_key(consumer));
            }
            if provider_id == consumer_id || (provider_id != &own.id && consumer_id != &own.id) {
                continue;
            }
            let identity = (
                (provider_id.clone(), row_key(provider)),
                (consumer_id.clone(), row_key(consumer)),
            );
            if pairs_seen.insert(identity) {
                links.push(CrossRepoLink {
                    basis: LinkBasis::Rpc(pair.basis),
                    provider: LinkEndpoint::from_row(provider_name, provider),
                    consumer: LinkEndpoint::from_row(consumer_name, consumer),
                });
            }
        }
    }

    // Statuses: one of linked / orphan / unscoped for every own consumer.
    let mut status = HashMap::new();
    for row in own_rows.iter().filter(|r| r.role == ContractRole::Consumer) {
        let key = row_key(row);
        let exact = own.providers_by_id.contains_key(&row.canonical_id)
            || members
                .iter()
                .any(|m| m.providers_by_id.contains_key(&row.canonical_id));
        let st = if exact || linked_consumer_keys.contains(&key) {
            ConsumerStatus::Linked
        } else if !scope.declared.is_empty() {
            ConsumerStatus::Orphan
        } else {
            ConsumerStatus::Unscoped
        };
        status.insert(key, st);
    }

    // Unused providers (REQ-007): own rows iterate in list order so the
    // result is deterministic without an extra sort.
    let mut unused_providers = Vec::new();
    for row in own_rows.iter().filter(|r| r.role == ContractRole::Provider) {
        let consumed = own.consumers_by_id.contains_key(&row.canonical_id)
            || members
                .iter()
                .any(|m| m.consumers_by_id.contains_key(&row.canonical_id))
            || consumed_provider_keys.contains(&row_key(row));
        if !consumed {
            unused_providers.push(row.clone());
        }
    }

    links.sort_by(|a, b| {
        a.provider
            .repo
            .cmp(&b.provider.repo)
            .then_with(|| a.provider.file.cmp(&b.provider.file))
            .then_with(|| a.provider.line.cmp(&b.provider.line))
            .then_with(|| a.consumer.repo.cmp(&b.consumer.repo))
            .then_with(|| a.consumer.file.cmp(&b.consumer.file))
            .then_with(|| a.consumer.line.cmp(&b.consumer.line))
            .then_with(|| a.provider.canonical_id.cmp(&b.provider.canonical_id))
    });

    Ok(WorkspaceResolution {
        scope,
        siblings,
        links,
        status,
        unused_providers,
    })
}

impl RepoRows {
    /// All RPC-family rows, in deterministic order (for the relaxed join).
    fn grpc_rows(&self) -> Vec<ContractRow> {
        let mut rows: Vec<ContractRow> = self
            .providers_by_id
            .values()
            .flatten()
            .chain(self.consumers_by_id.values().flatten())
            .filter(|r| is_rpc_family(r.kind))
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            a.canonical_id
                .cmp(&b.canonical_id)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.role.as_str().cmp(b.role.as_str()))
        });
        rows
    }
}

/// Workspace membership snapshot for `wonk status` (REQ-021, AR-027).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceStatus {
    /// Verbatim declared ids (empty when undeclared).
    pub declared: Vec<String>,
    /// Normalized effective set.
    pub effective: Vec<String>,
    /// Workspaces stored in this repo's `meta.json`; `None` when unreadable.
    pub stored: Option<Vec<String>>,
    /// Names of same-workspace repos found in the registry.
    pub comembers: Vec<String>,
    /// The stored set no longer matches the declared set — a `wonk update`
    /// would publish it.
    pub stored_diverges: bool,
}

/// Build the workspace section of `wonk status`.
pub fn workspace_status(
    repos_dir: &std::path::Path,
    own_root: &std::path::Path,
    own_index: &std::path::Path,
    declared: &[String],
) -> WorkspaceStatus {
    let scope = workspace_scope(declared, own_root);
    let members = scan_registry(repos_dir, own_root, &scope.effective);
    let stored = crate::db::read_meta(own_index).ok().map(|m| m.workspaces);
    let stored_diverges = stored.as_ref().is_some_and(|s| s != &scope.declared);
    WorkspaceStatus {
        declared: scope.declared.clone(),
        effective: scope.effective.clone(),
        stored,
        comembers: members.into_iter().map(|m| m.name).collect(),
        stored_diverges,
    }
}

#[cfg(test)]
mod indexed_rpc_tests {
    use super::*;
    fn fact(service: &str, method: &str, role: ContractRole) -> ContractCandidate {
        ContractCandidate {
            kind: ContractKind::Grpc,
            role,
            qualifier: service.into(),
            identifier: method.into(),
            canonical_id: canonical_contract_id(ContractKind::Grpc, service, method),
            params: vec![],
            owning_symbol: None,
            line: 1,
            confidence: 1.0,
        }
    }
    #[test]
    fn audit_f20_provider_probes_are_linear() {
        let count = 128;
        let providers: Vec<_> = (0..count)
            .map(|n| {
                fact(
                    &format!("users.v1.Service{n}"),
                    "GetUser",
                    ContractRole::Provider,
                )
            })
            .collect();
        let consumers: Vec<_> = (0..count)
            .map(|n| fact(&format!("Service{n}"), "get_user", ContractRole::Consumer))
            .collect();
        let scopes = [
            RpcJoinScope {
                workspace: " shared ".into(),
                candidates: &providers,
            },
            RpcJoinScope {
                workspace: "SHARED".into(),
                candidates: &consumers,
            },
        ];
        let (pairs, probes) = rpc_join_refs(&scopes);
        assert_eq!(pairs.len(), count);
        assert!(
            probes <= 4 * (providers.len() + consumers.len()),
            "{probes} probes for {count} providers and consumers"
        );
    }
    #[test]
    fn audit_f20_index_matches_exhaustive_reference_with_ties_exclusions_and_workspaces() {
        let providers = [
            fact("pkg.S", "*", ContractRole::Provider),
            fact("first.S", "GetUser", ContractRole::Provider),
            fact("second.S", "get_user", ContractRole::Provider),
            fact("Exact", "Same", ContractRole::Provider),
            fact("pkg.T", "*", ContractRole::Provider),
        ];
        let consumers = [
            fact("S", "getUser", ContractRole::Consumer),
            fact("T", "unknown", ContractRole::Consumer),
            fact("Exact", "Same", ContractRole::Consumer),
            fact("missing", "GetUser", ContractRole::Consumer),
            fact("S", "getUser", ContractRole::Consumer),
        ];
        let other = [
            fact("foreign.S", "GetUser", ContractRole::Provider),
            fact("S", "getUser", ContractRole::Consumer),
        ];
        let excluded = [fact("S", "getUser", ContractRole::Provider)];
        for p in [&providers[..], &providers[1..], &providers[..1]] {
            for c in [&consumers[..], &consumers[1..]] {
                for exact in [&[][..], &excluded[..]] {
                    let scopes = [
                        RpcJoinScope {
                            workspace: " Shared ".into(),
                            candidates: p,
                        },
                        RpcJoinScope {
                            workspace: "shared".into(),
                            candidates: c,
                        },
                        RpcJoinScope {
                            workspace: "elsewhere".into(),
                            candidates: &other,
                        },
                        RpcJoinScope {
                            workspace: "SHARED".into(),
                            candidates: exact,
                        },
                    ];
                    assert_eq!(
                        canonical_rpc_join(&scopes),
                        canonical_rpc_join_reference(&scopes)
                    );
                }
            }
        }
    }
    fn canonical_rpc_join_reference(scopes: &[RpcJoinScope]) -> Vec<RpcJoin> {
        use std::collections::{HashMap, HashSet};

        // Exact ID sets per role and normalized workspace: a candidate whose
        // canonical ID has an OPPOSITE-ROLE counterpart in its own workspace
        // belongs to the first pass and never enters the join.
        let mut provider_ids: HashMap<String, HashSet<String>> = HashMap::new();
        let mut consumer_ids: HashMap<String, HashSet<String>> = HashMap::new();
        for scope in scopes {
            let ws = normalize_workspace_id(&scope.workspace);
            for cand in scope.candidates {
                if !is_rpc_family(cand.kind) {
                    continue;
                }
                let slot = if cand.role == ContractRole::Provider {
                    &mut provider_ids
                } else {
                    &mut consumer_ids
                };
                slot.entry(ws.clone())
                    .or_default()
                    .insert(cand.canonical_id.clone());
            }
        }

        // Participants, with their scope index for deterministic tie-breaks.
        let mut providers: Vec<(usize, String, &ContractCandidate)> = Vec::new();
        let mut consumers: Vec<(usize, String, &ContractCandidate)> = Vec::new();
        for (si, scope) in scopes.iter().enumerate() {
            let ws = normalize_workspace_id(&scope.workspace);
            for cand in scope.candidates.iter() {
                if !is_rpc_family(cand.kind) {
                    continue;
                }
                // Only an opposite-role exact counterpart excludes.
                let exact_other = match cand.role {
                    ContractRole::Provider => &consumer_ids,
                    ContractRole::Consumer => &provider_ids,
                };
                if exact_other
                    .get(&ws)
                    .is_some_and(|ids| ids.contains(&cand.canonical_id))
                {
                    continue;
                }
                let slot = if cand.role == ContractRole::Provider {
                    &mut providers
                } else {
                    &mut consumers
                };
                slot.push((si, ws.clone(), cand));
            }
        }

        let mut joins = Vec::new();
        for (csi, cws, consumer) in &consumers {
            // Best provider: method-level before service-level, then the lowest
            // (scope, candidate) index.
            let mut best: Option<(u8, &ContractCandidate, usize, RpcMatchBasis)> = None;
            for (psi, pws, provider) in &providers {
                if pws != cws {
                    continue;
                }
                let Some(basis) = rpc_relaxed_match(consumer, provider) else {
                    continue;
                };
                let rank = u8::from(basis != RpcMatchBasis::ServiceLevelProvider);
                let better = match best {
                    None => true,
                    // Strictly better rank only: equal rank keeps the earlier
                    // provider (lowest (scope, candidate) index).
                    Some((r, _, _, _)) => rank > r,
                };
                if better {
                    best = Some((rank, provider, *psi, basis));
                }
            }
            if let Some((_, provider, psi, basis)) = best {
                joins.push(RpcJoin {
                    provider: RpcJoinSide {
                        canonical_id: provider.canonical_id.clone(),
                        workspace: scopes[psi].workspace.clone(),
                        role: ContractRole::Provider,
                    },
                    consumer: RpcJoinSide {
                        canonical_id: consumer.canonical_id.clone(),
                        workspace: scopes[*csi].workspace.clone(),
                        role: ContractRole::Consumer,
                    },
                    basis,
                });
            }
        }
        joins
    }
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
