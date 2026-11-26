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
        if let Some(prompt) = args.get("prompt") {
            return Self::from_prompt(args, prompt);
        }
        Self::from_legacy_task(args)
    }

    fn from_prompt(args: &Value, prompt: &Value) -> Result<Self, String> {
        if args.get("task").is_some() {
            return Err("child start accepts either prompt or task".into());
        }
        let prompt = prompt
            .as_str()
            .filter(|value| !value.trim().is_empty() && value.len() <= 24_000)
            .ok_or("child prompt must contain 1-24000 bytes")?;
        let role = role(args)?;
        let task_preview = args
            .get("taskPreview")
            .and_then(Value::as_str)
            .ok_or("child prompt start requires taskPreview")?;
        if task_preview.chars().count() > 200 {
            return Err("child taskPreview must contain at most 200 characters".into());
        }
        Ok(Self {
            prompt: prompt.into(),
            role: role.into(),
            task_preview: task_preview.into(),
            turns: bounded_number(args, "maxTurns", None, 1, 32)? as u32,
            timeout: bounded_number(args, "timeoutSeconds", None, 10, 1800)?,
        })
    }

    // Published extensions still send a task. Keep their defaults and prompt
    // here until they can use the explicit-prompt contract.
    fn from_legacy_task(args: &Value) -> Result<Self, String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or("missing child task")?;
        let role = role(args)?;
        if task.trim().is_empty() || task.len() > 20_000 {
            return Err("child task must contain 1-20000 bytes".into());
        }
        let turns = bounded_number(args, "maxTurns", Some(8), 1, 32)? as u32;
        let timeout = bounded_number(args, "timeoutSeconds", Some(300), 10, 1800)?;
        Ok(Self {
            prompt: format!("You are a delegated {role} subagent. Complete the task in at most {turns} turns. Do not delegate, spawn, or ask another agent to do work. Work only in the current project and return a concise final report.\n\nTask:\n{task}"),
            role: role.into(),
            task_preview: task.chars().take(200).collect(),
            turns,
            timeout,
        })
    }
}

fn role(args: &Value) -> Result<&str, String> {
    let role = args
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("worker")
        .trim();
    if role.is_empty() || role.len() > 200 {
        return Err("child role must contain 1-200 bytes".into());
    }
    Ok(role)
}

fn bounded_number(
    args: &Value,
    key: &str,
    default: Option<u64>,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let value = match args.get(key) {
        None => default.ok_or_else(|| format!("missing child {key}"))?,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an integer"))?,
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{key} must be between {min} and {max}"));
    }
    Ok(value)
}
