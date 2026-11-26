use serde_json::Value;

pub(super) struct StartRequest {
    pub prompt: String,
    pub role: String,
    pub task_preview: String,
    pub turns: u32,
    pub timeout: u64,
}

impl StartRequest {
    pub(super) fn parse(args: &Value) -> Result<Self, String> {
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
        let turns = bounded_number(args, "maxTurns", 8, 1, 32)? as u32;
        let timeout = bounded_number(args, "timeoutSeconds", 300, 10, 1800)?;
        let prompt = match args.get("prompt") {
            Some(prompt) => prompt
                .as_str()
                .filter(|value| !value.trim().is_empty() && value.len() <= 24_000)
                .ok_or("child prompt must contain 1-24000 bytes")?
                .to_owned(),
            // Older extensions supply only the task and delegate prompt preparation to the host.
            None => format!("You are a delegated {role} subagent. Complete the task in at most {turns} turns. Do not delegate, spawn, or ask another agent to do work. Work only in the current project and return a concise final report.\n\nTask:\n{task}"),
        };
        Ok(Self {
            prompt,
            role: role.into(),
            task_preview: task.chars().take(200).collect(),
            turns,
            timeout,
        })
    }
}

fn bounded_number(
    args: &Value,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an integer"))?,
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{key} must be between {min} and {max}"));
    }
    Ok(value)
}
