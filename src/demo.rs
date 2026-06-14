//! A synthetic dependency graph used by `--demo`. It exercises every
//! interesting shape — fan-in, fan-out, a cross-project external blocker, and a
//! genuine cycle — so the TUI can be explored (and screenshotted) without a
//! Linear API key.

use crate::model::{Graph, Issue, Priority, Status};

fn issue(key: &str, title: &str, status: Status, priority: Priority, assignee: &str) -> Issue {
    Issue {
        key: key.to_string(),
        title: title.to_string(),
        status,
        priority,
        assignee: (!assignee.is_empty()).then(|| assignee.to_string()),
        external: false,
    }
}

pub fn graph() -> Graph {
    use Priority::*;
    use Status::*;

    let mut g = Graph::new("Inference Platform");

    for i in [
        issue(
            "ZAP-150",
            "Protobuf schema freeze",
            Completed,
            High,
            "r.okafor",
        ),
        issue(
            "ZAP-188",
            "gRPC transport upgrade",
            Started,
            Medium,
            "r.okafor",
        ),
        issue("ZAP-198", "Tokenizer v2 cache", Started, Urgent, "j.liang"),
        issue("ZAP-201", "GPU pool autoscaler", Started, Urgent, "m.singh"),
        issue(
            "ZAP-204",
            "Streaming token API",
            Started,
            Urgent,
            "r.okafor",
        ),
        issue("ZAP-205", "SSE backpressure", Started, None, "j.liang"),
        issue("ZAP-210", "Client SDK retries", Backlog, Low, ""),
        issue(
            "ZAP-212",
            "Multi-region failover",
            Unstarted,
            Urgent,
            "m.singh",
        ),
        issue("ZAP-233", "Docs: SSE examples", Backlog, Low, ""),
        issue("ZAP-240", "Token usage metering", Started, High, "j.liang"),
        issue("ZAP-251", "Rate-limit headers", Backlog, Low, ""),
    ] {
        g.add_issue(i);
    }

    // A cross-project blocker we only learn about through a relation.
    g.ensure_external("INFRA-77", "Terraform GPU module");

    // blocker → blocked
    let edges = [
        ("ZAP-150", "ZAP-188"),  // schema freeze → gRPC upgrade (blocker done)
        ("ZAP-198", "ZAP-204"),  // tokenizer cache → streaming
        ("ZAP-188", "ZAP-204"),  // gRPC upgrade → streaming
        ("ZAP-201", "ZAP-204"),  // autoscaler → streaming (fan-in)
        ("INFRA-77", "ZAP-201"), // external → autoscaler
        ("ZAP-204", "ZAP-205"),  // streaming → backpressure (fan-out)
        ("ZAP-204", "ZAP-240"),  // streaming → metering
        ("ZAP-204", "ZAP-233"),  // streaming → docs
        ("ZAP-205", "ZAP-210"),  // backpressure → SDK retries
        ("ZAP-240", "ZAP-251"),  // metering → rate-limit headers
        ("ZAP-240", "ZAP-212"),  // metering → failover
        ("ZAP-212", "ZAP-204"),  // failover → streaming  ⇒ cycle 204→240→212→204
    ];
    for (blocker, blocked) in edges {
        g.add_edge(blocker, blocked);
    }

    g.finalize();
    g
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Direction;

    /// Anchor the hand-maintained fixture so a stray edit to the edge list fails
    /// here with a clear message instead of rippling opaquely through the
    /// app/snapshot tests that all build on it.
    #[test]
    fn demo_graph_has_the_expected_shape() {
        let g = graph();
        assert_eq!(g.cycle_count(), 1);

        let mut members = g.cycle_members();
        members.sort();
        assert_eq!(members, vec!["ZAP-204", "ZAP-212", "ZAP-240"]);

        let externals = g.externals();
        assert_eq!(externals.len(), 1);
        assert_eq!(externals[0].key, "INFRA-77");

        // ZAP-204 sits on the cycle but must not count itself in its closure.
        assert_eq!(g.transitive("ZAP-204", Direction::Downstream), 6);
    }
}
