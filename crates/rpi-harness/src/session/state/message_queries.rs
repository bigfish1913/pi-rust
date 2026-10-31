//! Borrow metadata first; clone/serialize only the selected bounded payload.
use super::SessionState;
use crate::error::{SessionError, SessionErrorCode, SessionResult};
use crate::session::types::{
    Entry, MessageSummary, MessageSummaryPage, MessageSummaryQuery, MessageWindowPage,
    MessageWindowQuery,
};
use rpi_agent::message::AgentMessage;
use rpi_ai::types::{Content, UserContent};

fn invalid(message: &str) -> SessionError {
    SessionError::new(SessionErrorCode::InvalidQuery, message)
}
fn validate_limit(limit: usize) -> SessionResult<()> {
    if !(1..=100).contains(&limit) {
        return Err(invalid("limit must be between 1 and 100"));
    }
    Ok(())
}
fn validate_id(id: &str) -> SessionResult<()> {
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(invalid("invalid entry cursor"));
    }
    Ok(())
}

impl SessionState {
    // Parent indices always precede children: apply_mutation requires an existing
    // parent and a new ID. This walk needs neither payload clones nor a path Vec.
    fn query_is_ancestor(&self, ancestor: usize, mut leaf: usize) -> bool {
        loop {
            if ancestor == leaf {
                return true;
            }
            if leaf < ancestor {
                return false;
            }
            match self.entries[leaf]
                .parent_id()
                .and_then(|id| self.entries_by_id.get(id))
            {
                Some(parent) => leaf = *parent,
                None => return false,
            }
        }
    }
    fn query_leaf(&self, lane: &str, requested: Option<&str>) -> SessionResult<Option<usize>> {
        let selected = self
            .lanes
            .get(lane)
            .ok_or_else(|| SessionError::new(SessionErrorCode::InvalidLane, "invalid lane"))?;
        let current = selected
            .as_ref()
            .and_then(|id| self.entries_by_id.get(id))
            .copied();
        let Some(id) = requested else {
            return Ok(current);
        };
        validate_id(id)?;
        let pinned = self
            .entries_by_id
            .get(id)
            .copied()
            .ok_or_else(|| invalid("branch_changed: snapshot leaf not found"))?;
        if !current.is_some_and(|current| self.query_is_ancestor(pinned, current)) {
            return Err(invalid(
                "branch_changed: snapshot leaf is no longer on the selected branch",
            ));
        }
        Ok(Some(pinned))
    }
    fn previous_user(&self, index: usize) -> Option<usize> {
        self.entries[index]
            .parent_id()
            .and_then(|id| self.entries_by_id.get(id))
            .and_then(|parent| self.nearest_user[*parent])
    }
    pub fn message_summaries(
        &self,
        lane: &str,
        query: &MessageSummaryQuery,
    ) -> SessionResult<MessageSummaryPage> {
        validate_limit(query.limit)?;
        if query.role != "user" {
            return Err(invalid("only role=user is supported"));
        }
        let leaf = self.query_leaf(lane, query.leaf_id.as_deref())?;
        let mut next = leaf.and_then(|index| self.nearest_user[index]);
        if let Some(before) = query.before_id.as_deref() {
            validate_id(before)?;
            let cursor = self
                .entries_by_id
                .get(before)
                .copied()
                .ok_or_else(|| invalid("user cursor not found"))?;
            if !matches!(
                self.entries[cursor].as_message(),
                Some(AgentMessage::User(_))
            ) {
                return Err(invalid("cursor must identify a user message"));
            }
            // Membership walks only the user chain, not intervening tools.
            while let Some(index) = next {
                if index == cursor {
                    break;
                }
                next = self.previous_user(index);
            }
            if next.is_none() {
                return Err(invalid(
                    "user cursor is not on the selected branch snapshot",
                ));
            }
            next = self.previous_user(cursor);
        }
        let mut selected = Vec::with_capacity(query.limit);
        while let Some(index) = next {
            if selected.len() == query.limit {
                break;
            }
            selected.push(index);
            next = self.previous_user(index);
        }
        let has_more = next.is_some();
        selected.reverse();
        let messages: Vec<_> = selected
            .into_iter()
            .map(|index| {
                let entry = &self.entries[index];
                let Some(AgentMessage::User(user)) = entry.as_message() else {
                    unreachable!("user index invariant")
                };
                let mut preview = String::new();
                let mut remaining = 2048usize;
                let mut count = 0;
                let mut append = |text: &str| {
                    for ch in text.chars() {
                        if remaining == 0 || count >= 140 {
                            break;
                        }
                        remaining -= 1;
                        if ch.is_whitespace() {
                            if !preview.is_empty() && !preview.ends_with(' ') {
                                preview.push(' ');
                                count += 1;
                            }
                        } else {
                            preview.push(ch);
                            count += 1;
                        }
                    }
                };
                let has_images = match &user.content {
                    UserContent::Text(text) => {
                        append(text);
                        false
                    }
                    UserContent::Blocks(blocks) => {
                        for block in blocks {
                            if let Content::Text(text) = block {
                                append(&text.text);
                                append(" ");
                            }
                        }
                        blocks
                            .iter()
                            .any(|block| matches!(block, Content::Image(_)))
                    }
                };
                MessageSummary {
                    entry_id: entry.id().into(),
                    role: "user".into(),
                    preview: preview.trim_end().into(),
                    timestamp: user.timestamp,
                    has_images,
                }
            })
            .collect();
        let first_id = messages.first().map(|message| message.entry_id.clone());
        let last_id = messages.last().map(|message| message.entry_id.clone());
        Ok(MessageSummaryPage {
            filtered: true,
            role: "user".into(),
            next_before_id: has_more.then(|| first_id.clone()).flatten(),
            first_id,
            last_id,
            messages,
            has_more,
            leaf_id: leaf.map(|index| self.entries[index].id().into()),
        })
    }
    pub fn message_window(
        &self,
        lane: &str,
        query: &MessageWindowQuery,
    ) -> SessionResult<MessageWindowPage> {
        validate_limit(query.limit)?;
        let cursors = [
            query.anchor_id.as_deref(),
            query.before_id.as_deref(),
            query.after_id.as_deref(),
        ];
        if cursors.iter().filter(|id| id.is_some()).count() != 1 {
            return Err(invalid(
                "exactly one of anchorId, beforeId or afterId is required",
            ));
        }
        let cursor = cursors.into_iter().flatten().next().unwrap();
        validate_id(cursor)?;
        let leaf = self.query_leaf(lane, query.leaf_id.as_deref())?;
        // Validate membership while retaining only the closest newer messages,
        // never a full branch path or unselected payload. Near-tail queries stop
        // after the requested older neighbourhood rather than walking to root.
        let mut newer = std::collections::VecDeque::with_capacity(query.limit + 1);
        let mut discarded_newer = false;
        let mut next = leaf;
        let mut anchor = None;
        while let Some(index) = next {
            let entry = &self.entries[index];
            next = entry
                .parent_id()
                .and_then(|id| self.entries_by_id.get(id))
                .copied();
            if entry.id() == cursor {
                if !matches!(entry, Entry::Message(_) | Entry::Compaction(_)) {
                    return Err(invalid("cursor must identify a transcript message"));
                }
                anchor = Some(index);
                break;
            }
            if matches!(entry, Entry::Message(_) | Entry::Compaction(_)) {
                newer.push_back(index);
                if newer.len() > query.limit + 1 {
                    newer.pop_front();
                    discarded_newer = true;
                }
            }
        }
        let anchor =
            anchor.ok_or_else(|| invalid("message cursor not on selected branch snapshot"))?;
        let newer: Vec<_> = newer.into_iter().rev().collect();
        let mut older = Vec::with_capacity(query.limit + 1);
        while let Some(index) = next {
            let entry = &self.entries[index];
            next = entry
                .parent_id()
                .and_then(|id| self.entries_by_id.get(id))
                .copied();
            if matches!(entry, Entry::Message(_) | Entry::Compaction(_)) {
                older.push(index);
                if older.len() > query.limit {
                    break;
                }
            }
        }
        let (selected, mut has_more_before, mut has_more_after) = if query.before_id.is_some() {
            (
                older
                    .iter()
                    .take(query.limit)
                    .rev()
                    .copied()
                    .collect::<Vec<_>>(),
                older.len() > query.limit,
                true,
            )
        } else if query.after_id.is_some() {
            (
                newer.iter().take(query.limit).copied().collect::<Vec<_>>(),
                true,
                newer.len() > query.limit || discarded_newer,
            )
        } else {
            let before = older.len().min(query.limit / 3);
            let after = newer.len().min(query.limit - 1 - before);
            let before = older.len().min(query.limit - 1 - after);
            let mut selected = older.iter().take(before).rev().copied().collect::<Vec<_>>();
            selected.push(anchor);
            selected.extend(newer.iter().take(after).copied());
            (
                selected,
                older.len() > before,
                newer.len() > after || discarded_newer,
            )
        };
        let entries: Vec<_> = selected
            .into_iter()
            .map(|index| self.entries[index].clone())
            .collect();
        if entries.is_empty() {
            has_more_before = false;
            has_more_after = false;
        }
        let first_id = entries.first().map(|entry| entry.id().into());
        let last_id = entries.last().map(|entry| entry.id().into());
        Ok(MessageWindowPage {
            entries,
            next_before_id: has_more_before.then(|| first_id.clone()).flatten(),
            next_after_id: has_more_after.then(|| last_id.clone()).flatten(),
            first_id,
            last_id,
            has_more_before,
            has_more_after,
            leaf_id: leaf.map(|index| self.entries[index].id().into()),
        })
    }
}

#[cfg(test)]
mod tests;
