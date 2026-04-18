use tokio::task::JoinSet;

pub struct TaskManager {
    tasks: JoinSet<()>,
}

impl TaskManager {
    pub fn new(_max_concurrent: usize) -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    /// Waits for all tasks to complete
    pub async fn shutdown(&mut self) {
        log::info!("Shutting down TaskManager, waiting for {} tasks", self.tasks.len());
        while let Some(result) = self.tasks.join_next().await {
            if let Err(e) = result {
                log::error!("Task failed during shutdown: {:?}", e);
            }
        }
        log::info!("TaskManager shutdown complete");
    }

    /// Aborts all tasks without waiting
    pub fn abort_all(&mut self) {
        log::warn!("Aborting all {} tasks", self.tasks.len());
        self.tasks.abort_all();
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        if !self.tasks.is_empty() {
            log::warn!("TaskManager dropped with {} active tasks, aborting them", self.tasks.len());
            self.abort_all();
        }
    }
}