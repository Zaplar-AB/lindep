//! Minimal synchronous Linear GraphQL client. Resolves a project by name and
//! pages through its issues, collecting `blocks` relations from both
//! directions into a [`Graph`].
//!
//! Auth: a personal API key (`lin_api_…`) is sent verbatim in the
//! `Authorization` header — **not** as a `Bearer` token (that form is for OAuth
//! and yields 401 here).

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{Graph, Issue, Priority, Status};

const ENDPOINT: &str = "https://api.linear.app/graphql";
// Linear caps single-query complexity at 10k points, and nested connections
// multiply (issues × relations × inverseRelations). Keep all three small; any
// issue that overflows the inline relation cap is topped up by `page_relations`.
const PAGE: u32 = 25; // issues per page
const REL_PAGE: u32 = 20; // relations per direction, inline

pub struct Client {
    agent: ureq::Agent,
    api_key: String,
}

#[derive(Serialize)]
struct Request<'a> {
    query: &'a str,
    variables: serde_json::Value,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize)]
struct GqlError {
    message: String,
}

impl Client {
    pub fn new(api_key: String) -> Self {
        // Don't treat non-2xx as a transport error — Linear returns HTTP 400
        // with a descriptive GraphQL `errors` body we want to surface.
        let agent = ureq::config::Config::builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        Client { agent, api_key }
    }

    fn execute<T: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T, String> {
        let body = Request { query, variables };
        let mut resp = self
            .agent
            .post(ENDPOINT)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| format!("request to Linear failed: {e}"))?;

        let status = resp.status().as_u16();
        if status == 401 {
            return Err("401 Unauthorized — check LINEAR_API_KEY (use the raw \
                        lin_api_… key, no \"Bearer\" prefix)"
                .to_string());
        }

        let envelope: Envelope<T> = match resp.body_mut().read_json() {
            Ok(env) => env,
            // A non-JSON body on a failure status is a gateway/HTML error page, not
            // a deserialization bug — surface the HTTP status, not a serde error.
            Err(_) if status >= 400 => return Err(format!("Linear API returned HTTP {status}")),
            Err(e) => {
                return Err(format!(
                    "could not parse Linear response (HTTP {status}): {e}"
                ));
            }
        };

        if let Some(errors) = envelope.errors
            && !errors.is_empty()
        {
            let joined = errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!("Linear API error: {joined}"));
        }

        match envelope.data {
            Some(data) => Ok(data),
            None if status >= 400 => Err(format!("Linear API returned HTTP {status}")),
            None => Err("Linear returned no data".to_string()),
        }
    }

    /// List every project (name + id), paged.
    pub fn list_projects(&self) -> Result<Vec<ProjectRef>, String> {
        const Q: &str = r#"
            query($after: String) {
              projects(first: 100, after: $after) {
                nodes { id name }
                pageInfo { hasNextPage endCursor }
              }
            }"#;
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let data: ProjectsData = self.execute(Q, json!({ "after": after }))?;
            out.extend(data.projects.nodes);
            // Advance only on a real cursor; `hasNextPage` with a null cursor
            // would reset to page 1 and loop forever, so treat it as the end.
            match (
                data.projects.page_info.has_next_page,
                data.projects.page_info.end_cursor,
            ) {
                (true, Some(cursor)) => after = Some(cursor),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Resolve a project by (case-insensitive) name, fetching the project list
    /// first. See [`resolve_in`] for the matching rules.
    pub fn resolve_project(&self, name: &str) -> Result<ProjectRef, String> {
        resolve_in(&self.list_projects()?, name)
    }

    /// Fetch all issues of a project (paged) and assemble the dependency graph.
    pub fn fetch_graph(&self, project: &ProjectRef) -> Result<Graph, String> {
        const Q: &str = r#"
            query($id: String!, $first: Int!, $after: String, $rel: Int!) {
              project(id: $id) {
                issues(first: $first, after: $after, includeArchived: false) {
                  nodes {
                    identifier
                    title
                    priority
                    state { type }
                    assignee { displayName }
                    relations(first: $rel) {
                      nodes { type relatedIssue { identifier title } }
                      pageInfo { hasNextPage endCursor }
                    }
                    inverseRelations(first: $rel) {
                      nodes { type issue { identifier title } }
                      pageInfo { hasNextPage endCursor }
                    }
                  }
                  pageInfo { hasNextPage endCursor }
                }
              }
            }"#;

        let mut raw: Vec<RawIssue> = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let data: ProjectIssuesData = self.execute(
                Q,
                json!({ "id": project.id, "first": PAGE, "after": after, "rel": REL_PAGE }),
            )?;
            let conn = data
                .project
                .ok_or_else(|| "project not found".to_string())?
                .issues;
            raw.extend(conn.nodes);
            match (conn.page_info.has_next_page, conn.page_info.end_cursor) {
                (true, Some(cursor)) => after = Some(cursor),
                _ => break,
            }
        }

        // Top up any issue whose relations overflowed the inline page cap, so
        // the graph never silently drops edges for heavily-linked issues. Only
        // page on a real `Some(cursor)`: `hasNextPage` with a null cursor carries
        // nothing to page from, and `relations(after: null)` would restart from
        // page 1 and re-fetch duplicates — the same guard the project/issue
        // pagination uses.
        for ri in &mut raw {
            // Clone the trigger out first so the post-fetch `extend` doesn't
            // overlap the borrow of `page_info`.
            let rel_cursor = page_cursor(&ri.relations.page_info, &ri.identifier);
            if let Some(cursor) = rel_cursor {
                let more = self.page_relations(&ri.identifier, false, Some(cursor))?;
                ri.relations.nodes.extend(more);
            }
            let inv_cursor = page_cursor(&ri.inverse_relations.page_info, &ri.identifier);
            if let Some(cursor) = inv_cursor {
                let more = self.page_relations(&ri.identifier, true, Some(cursor))?;
                ri.inverse_relations.nodes.extend(more);
            }
        }

        Ok(build_graph(&project.name, raw))
    }

    /// Page the remaining relations of a single issue past the inline cap.
    fn page_relations(
        &self,
        identifier: &str,
        inverse: bool,
        mut after: Option<String>,
    ) -> Result<Vec<RelNode>, String> {
        let (field, endpoint) = if inverse {
            ("inverseRelations", "issue { identifier title }")
        } else {
            ("relations", "relatedIssue { identifier title }")
        };
        let query = format!(
            "query($id: String!, $after: String) {{ issue(id: $id) {{ \
             {field}(first: 50, after: $after) {{ nodes {{ type {endpoint} }} \
             pageInfo {{ hasNextPage endCursor }} }} }} }}"
        );
        let mut out = Vec::new();
        loop {
            let data: IssueRelData =
                self.execute(&query, json!({ "id": identifier, "after": after }))?;
            let Some(holder) = data.issue else {
                // The issue vanished (archived / deleted / permissions) between the
                // page fetch and this top-up. Don't silently drop its overflow
                // edges — the module guarantee is that we never do that quietly.
                eprintln!(
                    "lindep: warning: could not page all relations for {identifier}; \
                     some dependency edges may be missing"
                );
                break;
            };
            out.extend(holder.conn.nodes);
            match (
                holder.conn.page_info.has_next_page,
                holder.conn.page_info.end_cursor,
            ) {
                (true, Some(cursor)) => after = Some(cursor),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// The cursor to top up a relation connection from, or `None` if there is
/// nothing reliable to page. `hasNextPage` with a real `Some(cursor)` yields it;
/// `hasNextPage` with a null cursor carries nothing to page from — paging with
/// `after: null` would restart at page 1 and re-fetch duplicates — so it warns
/// (overflow edges are never dropped silently) and returns `None`. The same
/// `(true, Some)` guard the project/issue pagination uses.
fn page_cursor(info: &PageInfo, identifier: &str) -> Option<String> {
    match (info.has_next_page, &info.end_cursor) {
        (true, Some(cursor)) => Some(cursor.clone()),
        (true, None) => {
            eprintln!(
                "lindep: warning: could not page all relations for {identifier} \
                 (no cursor); some dependency edges may be missing"
            );
            None
        }
        (false, _) => None,
    }
}

/// Resolve a project by (case-insensitive) name over an already-fetched list.
/// Prefers an exact match, otherwise a unique substring match; reports ambiguity
/// or a no-match with an actionable message. Pure, so it is unit-tested directly.
fn resolve_in(projects: &[ProjectRef], name: &str) -> Result<ProjectRef, String> {
    let needle = name.to_lowercase();
    if let Some(exact) = projects.iter().find(|p| p.name.to_lowercase() == needle) {
        return Ok(exact.clone());
    }
    let matches: Vec<&ProjectRef> = projects
        .iter()
        .filter(|p| p.name.to_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!(
            "no project matches \"{name}\". Run with --list to see all projects."
        )),
        many => {
            let names = many
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!("\"{name}\" is ambiguous — matches: {names}"))
        }
    }
}

/// Turn raw issues + relations into a finalized [`Graph`].
fn build_graph(project_name: &str, raw: Vec<RawIssue>) -> Graph {
    let mut graph = Graph::new(project_name);

    for ri in &raw {
        graph.add_issue(Issue {
            key: ri.identifier.clone(),
            title: ri.title.clone(),
            status: ri
                .state
                .as_ref()
                .map(|s| Status::from_type(&s.kind))
                .unwrap_or(Status::Unknown),
            priority: Priority::from_value(ri.priority.unwrap_or(0.0)),
            assignee: ri.assignee.as_ref().map(|a| a.display_name.clone()),
            external: false,
        });
    }

    // From `relations` this issue is the blocker (other = blocked); from
    // `inverseRelations` it is the blocked (other = blocker). Endpoints we never
    // fetched become external nodes.
    for ri in &raw {
        for rel in &ri.relations.nodes {
            if rel.kind == "blocks"
                && let Some(target) = &rel.other
            {
                graph.ensure_external(&target.identifier, &target.title);
                graph.add_edge(&ri.identifier, &target.identifier);
            }
        }
        for inv in &ri.inverse_relations.nodes {
            if inv.kind == "blocks"
                && let Some(source) = &inv.other
            {
                graph.ensure_external(&source.identifier, &source.title);
                graph.add_edge(&source.identifier, &ri.identifier);
            }
        }
    }

    graph.finalize();
    graph
}

// ── Wire types ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
struct ProjectsData {
    projects: ProjectConn,
}

#[derive(Deserialize)]
struct ProjectConn {
    nodes: Vec<ProjectRef>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct ProjectIssuesData {
    project: Option<ProjectIssues>,
}

#[derive(Deserialize)]
struct ProjectIssues {
    issues: IssueConn,
}

#[derive(Deserialize)]
struct IssueConn {
    nodes: Vec<RawIssue>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssue {
    identifier: String,
    title: String,
    priority: Option<f64>,
    state: Option<StateNode>,
    assignee: Option<AssigneeNode>,
    relations: RelConn,
    inverse_relations: RelConn,
}

#[derive(Deserialize)]
struct StateNode {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssigneeNode {
    display_name: String,
}

/// A relation connection — used for both `relations` and `inverseRelations`.
#[derive(Deserialize)]
struct RelConn {
    nodes: Vec<RelNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

/// One relation. The "other endpoint" arrives under different field names in
/// the two directions (`relatedIssue` vs `issue`); a serde alias unifies them.
#[derive(Deserialize)]
struct RelNode {
    #[serde(rename = "type")]
    kind: String,
    #[serde(alias = "relatedIssue", alias = "issue")]
    other: Option<IssueRef>,
}

#[derive(Deserialize)]
struct IssueRef {
    identifier: String,
    title: String,
}

/// Wrapper for the single-issue relation-paging follow-up query.
#[derive(Deserialize)]
struct IssueRelData {
    issue: Option<RelHolder>,
}

#[derive(Deserialize)]
struct RelHolder {
    #[serde(alias = "relations", alias = "inverseRelations")]
    conn: RelConn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Direction;
    use serde_json::{from_value, json};

    /// Deserialize a GraphQL-shaped `nodes` array into wire issues, exactly as
    /// the live query would, so `build_graph` is exercised without a network.
    fn issues(nodes: serde_json::Value) -> Vec<RawIssue> {
        from_value(nodes).expect("fixture deserializes as Vec<RawIssue>")
    }

    fn empty_rel() -> serde_json::Value {
        json!({ "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null } })
    }

    #[test]
    fn build_graph_orients_blocks_edges_and_collapses_the_mirror() {
        // A blocks B, reported from BOTH sides (A.relations and B.inverseRelations).
        let g = build_graph(
            "proj",
            issues(json!([
                {
                    "identifier": "A", "title": "Ay", "priority": 1.0,
                    "state": { "type": "started" }, "assignee": { "displayName": "x" },
                    "relations": { "nodes": [
                        { "type": "blocks", "relatedIssue": { "identifier": "B", "title": "Bee" } }
                    ], "pageInfo": { "hasNextPage": false, "endCursor": null } },
                    "inverseRelations": empty_rel()
                },
                {
                    "identifier": "B", "title": "Bee", "priority": 0.0,
                    "state": { "type": "unstarted" }, "assignee": null,
                    "relations": empty_rel(),
                    "inverseRelations": { "nodes": [
                        { "type": "blocks", "issue": { "identifier": "A", "title": "Ay" } }
                    ], "pageInfo": { "hasNextPage": false, "endCursor": null } }
                }
            ])),
        );
        assert_eq!(
            g.edge_count(),
            1,
            "the mirrored relation collapses to one edge"
        );
        assert_eq!(g.neighbours("A", Direction::Downstream), &["B"]);
        assert_eq!(g.neighbours("B", Direction::Upstream), &["A"]);
    }

    #[test]
    fn build_graph_ignores_non_blocks_relations() {
        let g = build_graph(
            "proj",
            issues(json!([
                {
                    "identifier": "A", "title": "Ay", "priority": null,
                    "state": null, "assignee": null,
                    "relations": { "nodes": [
                        { "type": "related", "relatedIssue": { "identifier": "B", "title": "Bee" } },
                        { "type": "duplicate", "relatedIssue": { "identifier": "C", "title": "Cee" } }
                    ], "pageInfo": { "hasNextPage": false, "endCursor": null } },
                    "inverseRelations": empty_rel()
                }
            ])),
        );
        assert_eq!(g.edge_count(), 0);
        assert!(g.neighbours("A", Direction::Downstream).is_empty());
    }

    #[test]
    fn build_graph_materializes_external_endpoints() {
        // A is blocked by INFRA-9, which is not among the fetched issues.
        let g = build_graph(
            "proj",
            issues(json!([
                {
                    "identifier": "A", "title": "Ay", "priority": null,
                    "state": { "type": "started" }, "assignee": null,
                    "relations": empty_rel(),
                    "inverseRelations": { "nodes": [
                        { "type": "blocks", "issue": { "identifier": "INFRA-9", "title": "ext" } }
                    ], "pageInfo": { "hasNextPage": false, "endCursor": null } }
                }
            ])),
        );
        let ext = g.get("INFRA-9").expect("external endpoint is materialized");
        assert!(ext.external);
        assert_eq!(ext.title, "ext");
        assert_eq!(g.neighbours("A", Direction::Upstream), &["INFRA-9"]);
    }

    fn proj(id: &str, name: &str) -> ProjectRef {
        ProjectRef {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn resolve_prefers_exact_over_substring() {
        let projects = [proj("1", "Infra"), proj("2", "Infrastructure")];
        assert_eq!(resolve_in(&projects, "infra").unwrap().name, "Infra");
    }

    #[test]
    fn resolve_unique_substring_matches() {
        let projects = [proj("1", "Core PMS"), proj("2", "Billing")];
        assert_eq!(resolve_in(&projects, "core").unwrap().id, "1");
    }

    #[test]
    fn resolve_ambiguous_lists_every_candidate() {
        let projects = [proj("1", "Inference Platform"), proj("2", "Infra")];
        let err = resolve_in(&projects, "inf").unwrap_err();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("Inference Platform") && err.contains("Infra"));
    }

    #[test]
    fn resolve_no_match_suggests_list() {
        let projects = [proj("1", "Core PMS")];
        let err = resolve_in(&projects, "zzz").unwrap_err();
        assert!(err.contains("--list"));
    }

    fn page_info(has_next: bool, cursor: Option<&str>) -> PageInfo {
        PageInfo {
            has_next_page: has_next,
            end_cursor: cursor.map(str::to_string),
        }
    }

    #[test]
    fn page_cursor_yields_a_real_cursor() {
        let info = page_info(true, Some("cur-42"));
        assert_eq!(page_cursor(&info, "ENG-1"), Some("cur-42".to_string()));
    }

    #[test]
    fn page_cursor_stops_when_there_is_no_next_page() {
        // The common case: relations fit inline. No top-up, regardless of cursor.
        assert_eq!(page_cursor(&page_info(false, None), "ENG-1"), None);
        assert_eq!(page_cursor(&page_info(false, Some("x")), "ENG-1"), None);
    }

    #[test]
    fn page_cursor_does_not_restart_from_page_one_on_a_null_cursor() {
        // The bug guarded here: `hasNextPage: true` with `endCursor: null` must
        // NOT page (which would `after: null` → re-fetch page 1 and duplicate
        // every relation). Returning None leaves the inline page as-is.
        assert_eq!(page_cursor(&page_info(true, None), "ENG-1"), None);
    }
}
