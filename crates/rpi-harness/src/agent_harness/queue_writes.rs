//! Queue ACKs and in-memory publication share an owned message gate. Dropping
//! an API caller cannot abandon the second half after storage accepted a write.
use super::*;

pub(super) struct NextRunReservation {
    pub(super) items: Vec<QueuedMessage>,
    queue: SharedMessageQueue,
}
impl Drop for NextRunReservation {
    fn drop(&mut self) {
        let mut queue = self.queue.lock().unwrap();
        for item in &self.items {
            queue.reserved.remove(&item.entry_id);
        }
    }
}

impl AgentHarness {
    pub(super) async fn reserve_next_run(&self) -> NextRunReservation {
        let _gate = self.message_writes.lock().await;
        let queue = self.inner.lock().unwrap().next_run_queue.clone();
        let items = {
            let mut q = queue.lock().unwrap();
            let items: Vec<_> = q
                .pending
                .iter()
                .filter(|item| item.lane == self.lane && !q.reserved.contains(&item.entry_id))
                .cloned()
                .collect();
            for item in &items {
                q.reserved.insert(item.entry_id.clone());
            }
            items
        };
        NextRunReservation { items, queue }
    }

    fn confirm_next_run(&self, id: &str) {
        let queue = self.inner.lock().unwrap().next_run_queue.clone();
        let mut q = queue.lock().unwrap();
        q.remove(id);
        q.reserved.remove(id);
    }

    pub(super) async fn commit_initial_prompt(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
    ) -> HarnessResult<Entry> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            let (_, committed) = harness
                .session
                .append_record_and_entry(record, entry, &harness.lane)
                .await
                .map_err(session_to_harness_err)?;
            harness.confirm_next_run(committed.id());
            Ok(committed)
        })
        .await
        .map_err(|error| HarnessError::io(format!("initial prompt write task failed: {error}")))?
    }

    pub(super) async fn commit_reserved_prompt(
        &self,
        entry: ProvisionedEntry,
    ) -> HarnessResult<Entry> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            let committed = harness.append_provisioned_locked(entry).await?;
            harness.confirm_next_run(committed.id());
            Ok(committed)
        })
        .await
        .map_err(|error| HarnessError::io(format!("prompt write task failed: {error}")))?
    }

    pub(super) async fn cancel_queue_item(&self, id: &str) -> HarnessResult<CancelQueuedResult> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _gate = gate;
            let queues = {
                let inner = harness.inner.lock().unwrap();
                if inner.closed {
                    return Err(HarnessError::closed());
                }
                [
                    inner.steering_queue.clone(),
                    inner.follow_up_queue.clone(),
                    inner.next_run_queue.clone(),
                ]
            };
            for queue in queues {
                let present = {
                    let q = queue.lock().unwrap();
                    q.pending
                        .iter()
                        .any(|item| item.lane == harness.lane && item.entry_id == id)
                        && !q.reserved.contains(&id)
                };
                if !present {
                    continue;
                }
                let record = harness.queue_cancellation(&id).await?;
                harness
                    .session
                    .append_queue_cancellations(vec![record])
                    .await
                    .map_err(session_to_harness_err)?;
                queue.lock().unwrap().remove(&id);
                return Ok(CancelQueuedResult {
                    outcome: CancelQueuedOutcome::Cancelled,
                });
            }
            Ok(CancelQueuedResult {
                outcome: CancelQueuedOutcome::AlreadyConsumed,
            })
        })
        .await
        .map_err(|error| HarnessError::io(format!("queue cancellation task failed: {error}")))?
    }

    async fn queue_cancellation(&self, id: &str) -> HarnessResult<LaneRecord> {
        let enqueued = self
            .find_enqueue(id)
            .await?
            .ok_or_else(|| HarnessError::io("queued item has no pending enqueue record"))?;
        Ok(LaneRecord::QueueCancelled(QueueCancelledRecord {
            base: self.record_base(),
            run_id: enqueued.run_id,
            entry_id: id.to_owned(),
        }))
    }

    pub(super) async fn clear_queue_items(&self) -> HarnessResult<QueuedMessages> {
        let gate = self.message_writes.clone().lock_owned().await;
        let harness = self.clone();
        tokio::spawn(async move {
            let _gate = gate;
            let queues = {
                let inner = harness.inner.lock().unwrap();
                if inner.closed {
                    return Err(HarnessError::closed());
                }
                [inner.steering_queue.clone(), inner.follow_up_queue.clone()]
            };
            let items: Vec<Vec<QueuedMessage>> = queues
                .iter()
                .map(|queue| {
                    queue
                        .lock()
                        .unwrap()
                        .pending
                        .iter()
                        .filter(|item| item.lane == harness.lane)
                        .cloned()
                        .collect()
                })
                .collect();
            let mut records = Vec::new();
            for item in items.iter().flatten() {
                records.push(harness.queue_cancellation(&item.entry_id).await?);
            }
            if !records.is_empty() {
                harness
                    .session
                    .append_queue_cancellations(records)
                    .await
                    .map_err(session_to_harness_err)?;
            }
            let mut restored = QueuedMessages::default();
            for (index, (queue, items)) in queues.iter().zip(&items).enumerate() {
                for item in items {
                    queue.lock().unwrap().remove(&item.entry_id);
                    let out = if index == 0 {
                        &mut restored.steering
                    } else {
                        &mut restored.follow_up
                    };
                    out.push(queued_message_text(&item.message));
                    if let AgentMessage::User(user) = &item.message {
                        if let UserContent::Blocks(blocks) = &user.content {
                            restored
                                .images
                                .extend(blocks.iter().filter_map(|block| match block {
                                    Content::Image(image) => Some(image.clone()),
                                    _ => None,
                                }));
                        }
                    }
                }
            }
            Ok(restored)
        })
        .await
        .map_err(|error| HarnessError::io(format!("queue clear task failed: {error}")))?
    }
}
