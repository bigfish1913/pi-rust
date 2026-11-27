//! Wire compatibility for the published ChildAgent action.
use super::{bounded_number, parse_tools, AgentHarness, IndependentRuns, Start, Value};

pub(super) struct LegacyRoster {
    role: String,
    task_preview: String,
}

impl LegacyRoster {
    pub(super) fn populate_row(&self, row: &mut Value) {
        row["role"] = Value::String(self.role.clone());
        row["taskPreview"] = Value::String(self.task_preview.clone());
    }
}

impl IndependentRuns {
    pub(in crate::extensions_actions) async fn start_child(
        &self,
        parent: &AgentHarness,
        owner: u64,
        args: &Value,
    ) -> Result<Value, String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or("missing child task")?;
        let role = args
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("worker")
            .trim();
        if task.trim().is_empty() || task.len() > 20_000 || role.is_empty() || role.len() > 200 {
            return Err("child task must contain 1-20000 bytes and role 1-200 bytes".into());
        }
        let turns = bounded_number(args, "maxTurns", Some(8), 0, 32)? as u32;
        let timeout = bounded_number(args, "timeoutSeconds", Some(300), 10, 1800)?;
        let turn_budget_instruction = if turns == 0 {
            "There is no fixed turn limit; complete the task within the host timeout.".to_string()
        } else {
            format!("Complete the task in at most {turns} turns.")
        };
        let input = format!(
            "You are a delegated {role} subagent. {turn_budget_instruction} Do not delegate, spawn, or ask another agent to do work. Work only in the current project and return a concise final report.\n\nTask:\n{task}"
        );
        self.start(
            parent,
            owner,
            Start {
                input,
                turns,
                timeout,
                tools: parse_tools(args)?,
                legacy: Some(LegacyRoster {
                    role: role.into(),
                    task_preview: task.chars().take(200).collect(),
                }),
            },
        )
        .await
        .map(child_reply)
    }

    pub(in crate::extensions_actions) fn handle_child(
        &self,
        owner: u64,
        args: &Value,
    ) -> Result<Value, String> {
        let op = args
            .get("op")
            .and_then(Value::as_str)
            .ok_or("missing child op")?;
        let id = args
            .get("childId")
            .and_then(Value::as_str)
            .ok_or("missing childId")?;
        self.handle_existing(owner, op, id).map(child_reply)
    }
}

fn child_reply(mut reply: Value) -> Value {
    let object = reply.as_object_mut().expect("run replies are objects");
    if let Some(id) = object.remove("runId") {
        object.insert("childId".into(), id);
    }
    reply
}
