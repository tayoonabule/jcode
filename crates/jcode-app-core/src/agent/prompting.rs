use super::Agent;
use crate::logging;
use crate::message::{Message, ToolDefinition};

impl Agent {
    pub(super) fn log_prompt_prefix_accounting(
        &self,
        split: &crate::prompt::SplitSystemPrompt,
        tools: &[ToolDefinition],
    ) {
        let system_tokens = split.estimated_tokens();
        let tool_tokens = ToolDefinition::aggregate_prompt_token_estimate(tools);
        let prefix_tokens = system_tokens + tool_tokens;
        logging::info(&format!(
            "Prompt prefix estimate: total={} tokens (system={} tools={})",
            prefix_tokens, system_tokens, tool_tokens
        ));
    }

    pub(super) fn build_memory_prompt_nonblocking_shared(
        &self,
        messages: std::sync::Arc<[Message]>,
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        if !self.memory_enabled {
            return None;
        }

        let session_id = &self.session.id;

        let fresh_user_turn = crate::message::ends_with_fresh_user_turn(&messages);
        let pending = if fresh_user_turn {
            crate::memory::take_pending_memory(session_id)
        } else {
            None
        };

        // Use the persistent memory-agent pipeline as the single source of truth.
        // Running both this and the legacy MemoryManager background retrieval path
        // can prepare overlapping pending prompts for the same turn, which makes
        // memory injection feel overly aggressive.
        // Relevance results are consumed only at the start of a fresh user turn.
        // Enqueuing again after every tool result runs the local embedding model
        // for each provider continuation without creating an additional injection
        // opportunity. One update per user turn keeps memory current while avoiding
        // redundant 512-token inference during tool-heavy agent loops.
        if fresh_user_turn {
            crate::memory_agent::update_context_sync_with_dir(
                session_id,
                messages,
                self.session.working_dir.clone(),
            );
        }

        pending
    }

    fn append_current_turn_system_reminder(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        let Some(reminder) = self
            .current_turn_system_reminder
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        else {
            return;
        };

        if !split.dynamic_part.is_empty() {
            split.dynamic_part.push_str("\n\n");
        }
        split.dynamic_part.push_str("# System Reminder\n\n");
        split.dynamic_part.push_str(reminder);
    }

    /// Build split system prompt for better caching
    /// Returns static (cacheable) and dynamic (not cached) parts separately
    pub(super) fn build_system_prompt_split(
        &self,
        memory_prompt: Option<&str>,
    ) -> crate::prompt::SplitSystemPrompt {
        if let Some(ref override_prompt) = self.system_prompt_override {
            return crate::prompt::SplitSystemPrompt {
                static_part: override_prompt.clone(),
                dynamic_part: String::new(),
            };
        }

        let skills = self.current_skills_snapshot();
        let skill_prompt = self
            .active_skill
            .as_ref()
            .and_then(|name| skills.get(name).map(|skill| skill.get_prompt().to_string()));

        let available_skills: Vec<crate::prompt::SkillInfo> = self
            .current_skills_snapshot()
            .list()
            .iter()
            .map(|skill| crate::prompt::SkillInfo {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect();

        let working_dir = self
            .session
            .working_dir
            .as_ref()
            .map(std::path::PathBuf::from);

        let (mut split, _context_info) = crate::prompt::build_system_prompt_split_with_agents_md(
            skill_prompt.as_deref(),
            &available_skills,
            self.session.is_canary,
            memory_prompt,
            working_dir.as_deref(),
            self.agents_md_snapshot.clone(),
        );

        // Tool definitions are intentionally dynamic: MCP servers and provider
        // integrations can appear or disappear during a session. Keep a small
        // human-readable inventory alongside the provider tool schemas so the
        // model does not forget capabilities that are otherwise easy to miss,
        // especially when MCP definitions are exposed through mcp_search/call.
        let available_tools = self.locked_tools.as_deref().unwrap_or(&[]);
        append_tool_capability_reminder(&mut split, available_tools);

        self.append_current_turn_system_reminder(&mut split);
        crate::prompt::append_swarm_effort_directive(
            &mut split,
            self.provider.reasoning_effort().as_deref(),
        );

        split
    }

    /// Non-blocking memory prompt - takes pending result and spawns check for next turn
    #[cfg(test)]
    pub(super) fn build_memory_prompt_nonblocking(
        &self,
        messages: &[Message],
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        self.build_memory_prompt_nonblocking_shared(messages.to_vec().into(), _memory_event_tx)
    }
}

/// Summarize the session's dynamic tool surface for the model.
///
/// Only families that genuinely appear and disappear mid-session are listed:
/// MCP servers can connect or drop at any time, and orchestration tools depend
/// on the swarm configuration. Static tools are named in the provider tool
/// schemas already, so they only get a single trailing roster line.
fn append_tool_capability_reminder(
    split: &mut crate::prompt::SplitSystemPrompt,
    tools: &[crate::message::ToolDefinition],
) {
    if tools.is_empty() {
        return;
    }

    let mut mcp = Vec::new();
    let mut orchestration = Vec::new();
    let mut other = Vec::new();

    for tool in tools {
        let name = tool.name.as_str();
        if name.starts_with("mcp__") || matches!(name, "mcp_search" | "mcp_call") {
            mcp.push(name);
        } else if matches!(name, "swarm" | "communicate" | "spawn") {
            orchestration.push(name);
        } else {
            other.push(name);
        }
    }

    let mut lines = vec![
        "# Available capabilities".to_string(),
        "The following capabilities are active in this session. Use the exact tool names in the tool definitions; this list is a compact reminder and updates with the current tool surface.".to_string(),
    ];
    append_capability_line(&mut lines, "MCP", &mcp);
    append_capability_line(&mut lines, "Subagents and orchestration", &orchestration);
    if orchestration.contains(&"swarm") {
        lines.push(
            "- Provider routes: use `swarm` with `list_models` to see authenticated models and alternate-provider routes before spawning; do not assume the coordinator route is the only one available.".to_string(),
        );
    }
    if !other.is_empty() {
        lines.push(format!("- Other tools available: {}", other.join(", ")));
    }

    if !split.dynamic_part.is_empty() {
        split.dynamic_part.push_str("\n\n");
    }
    split.dynamic_part.push_str(&lines.join("\n"));
}

fn append_capability_line(lines: &mut Vec<String>, label: &str, names: &[&str]) {
    if !names.is_empty() {
        lines.push(format!("- {} tools available: {}", label, names.join(", ")));
    }
}

#[cfg(test)]
mod capability_reminder_tests {
    use super::append_tool_capability_reminder;
    use crate::message::ToolDefinition;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        }
    }

    #[test]
    fn capability_reminder_surfaces_dynamic_tool_families() {
        let mut prompt = crate::prompt::SplitSystemPrompt::default();
        append_tool_capability_reminder(
            &mut prompt,
            &[
                tool("mcp__slack__search_messages"),
                tool("swarm"),
                tool("read"),
            ],
        );

        assert!(prompt.dynamic_part.contains("MCP tools available"));
        assert!(prompt.dynamic_part.contains("mcp__slack__search_messages"));
        assert!(prompt.dynamic_part.contains("Subagents and orchestration"));
        assert!(prompt.dynamic_part.contains("Provider routes"));
        assert!(prompt.dynamic_part.contains("Other tools available: read"));
    }

    #[test]
    fn capability_reminder_is_compact_for_empty_tool_families() {
        let mut prompt = crate::prompt::SplitSystemPrompt::default();
        append_tool_capability_reminder(&mut prompt, &[tool("read")]);

        assert!(!prompt.dynamic_part.contains("MCP tools available"));
        assert!(!prompt.dynamic_part.contains("Subagents and orchestration"));
        assert!(prompt.dynamic_part.contains("Other tools available: read"));
    }

    #[test]
    fn capability_reminder_is_absent_without_tools() {
        let mut prompt = crate::prompt::SplitSystemPrompt::default();
        append_tool_capability_reminder(&mut prompt, &[]);

        assert!(prompt.dynamic_part.is_empty());
    }
}
