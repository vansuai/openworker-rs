//! Pinned MCP tool allowlists for connector-backed MCP servers.
//!
//! Mirrors `coworker/connectors/tool_defs.py::mcp_pinned_tools` — vendor tool
//! names (prefix stripped) that seed `include_tools` on mcp-connect so drift
//! can only shrink capability, never grow it.

/// Vendor-side tool names pinned for a connector (empty → no pin / list all).
pub fn mcp_pinned_tools(connector: &str) -> &'static [&'static str] {
    match connector {
        "jira" => &[
            "getVisibleJiraProjects",
            "searchJiraIssuesUsingJql",
            "getJiraIssue",
            "getTransitionsForJiraIssue",
            "createJiraIssue",
            "editJiraIssue",
            "addCommentToJiraIssue",
            "transitionJiraIssue",
        ],
        "monday" => &[
            "get_user_context",
            "search",
            "get_board_info",
            "get_board_items_page",
            "board_insights",
            "get_updates",
            "create_item",
            "change_item_column_values",
            "create_update",
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jira_and_monday_pinned() {
        assert!(mcp_pinned_tools("jira").contains(&"getJiraIssue"));
        assert!(mcp_pinned_tools("monday").contains(&"create_item"));
        assert!(mcp_pinned_tools("slack").is_empty());
    }
}
