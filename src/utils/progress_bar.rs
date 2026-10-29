use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use teloxide::{
    prelude::*,
    requests::Requester,
    types::{ChatId, MessageId},
};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

const MIN_UPDATE_INTERVAL: Duration = Duration::from_secs(3);

struct ProgressBarInner {
    bot: Bot,
    chat_id: ChatId,
    message_id: Option<MessageId>,
    last_update: Option<Instant>,
    last_percentage: u8,
    /// A silent bar never talks to Telegram: background batch items use it so
    /// they cannot create status bubbles of their own.
    enabled: bool,
    /// Keep the message after the transfer finishes. Batch items share one
    /// status message owned by the batch, not by the individual video.
    keep_message: bool,
    /// Batch only: the position whose progress may be drawn right now. A
    /// shared bar follows the earliest undelivered video, so the next item's
    /// download progress is visible while the previous one is still sending.
    active_position: Option<Arc<AtomicI64>>,
    /// Batch only: the last rendered text of every video, including videos
    /// that were downloading silently. When the message is handed over, the
    /// next video's last known state is drawn at once instead of waiting for
    /// a progress event that may never come (small clips finish downloading
    /// before their turn).
    last_by_position: HashMap<i64, (u8, String)>,
}

#[derive(Clone)]
pub struct ProgressBar {
    inner: Arc<Mutex<ProgressBarInner>>,
    /// Which batch position this handle reports for. Zero for single links.
    position: i64,
    /// Whether updates must match `active_position`.
    gated: bool,
}

impl ProgressBar {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self::create(bot, chat_id, true, false, 0, false, None)
    }

    /// No-op bar: every update is dropped, `start`/`delete` never reach the
    /// network. Used for background prefetch items and for deliveries whose
    /// feedback already exists (the ad choice message).
    pub fn silent() -> Self {
        Self::create(
            Bot::new("DUMMY_TOKEN"),
            ChatId(0),
            false,
            false,
            0,
            false,
            None,
        )
    }

    pub fn new_silent() -> Self {
        Self::silent()
    }

    /// The one status message of a burst. Position 1 draws it; every later
    /// item gets a gated handle that only draws while it is the active video.
    pub fn batch(bot: Bot, chat_id: ChatId, active_position: Arc<AtomicI64>) -> Self {
        Self::create(bot, chat_id, true, true, 1, true, Some(active_position))
    }

    pub fn for_position(&self, position: i64) -> Self {
        Self {
            inner: self.inner.clone(),
            position,
            gated: self.gated,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        bot: Bot,
        chat_id: ChatId,
        enabled: bool,
        keep_message: bool,
        position: i64,
        gated: bool,
        active_position: Option<Arc<AtomicI64>>,
    ) -> Self {
        ProgressBar {
            inner: Arc::new(Mutex::new(ProgressBarInner {
                bot,
                chat_id,
                message_id: None,
                last_update: None,
                last_percentage: 0,
                enabled,
                keep_message,
                active_position,
                last_by_position: HashMap::new(),
            })),
            position,
            gated,
        }
    }

    pub async fn message_id(&self) -> Option<MessageId> {
        self.inner.lock().await.message_id
    }

    /// Hand the shared message to the next video. Its last known state is
    /// drawn immediately — a clip that finished downloading while another was
    /// sending would otherwise show nothing until its upload starts.
    pub async fn hand_over(&mut self) {
        let mut inner = self.inner.lock().await;
        inner.last_update = None;
        inner.last_percentage = 0;
        if !inner.enabled {
            return;
        }
        let Some(active) = inner
            .active_position
            .as_ref()
            .map(|active| active.load(Ordering::Relaxed))
        else {
            return;
        };
        let Some((percentage, text)) = inner.last_by_position.get(&active).cloned() else {
            return;
        };
        let Some(message_id) = inner.message_id else {
            return;
        };
        if inner
            .bot
            .edit_message_text(inner.chat_id, message_id, &text)
            .await
            .is_ok()
        {
            log::info!("Progress handed over to video {} ({}%)", active, percentage);
            inner.last_percentage = percentage;
            inner.last_update = Some(Instant::now());
        }
    }

    pub async fn start(&mut self, initial_text: &str) -> Result<(), anyhow::Error> {
        let mut inner = self.inner.lock().await;
        if !inner.enabled {
            return Ok(());
        }
        if inner.message_id.is_some() {
            // A batch already owns the message. Starting the upload phase is a
            // new stage, so let its (lower) percentage render right away.
            inner.last_update = None;
            inner.last_percentage = 0;
            return Ok(());
        }
        let msg = inner.bot.send_message(inner.chat_id, initial_text).await?;
        inner.message_id = Some(msg.id);
        inner.last_update = Some(Instant::now());
        Ok(())
    }

    pub async fn update(
        &mut self,
        percentage: u8,
        extrainfo: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let mut inner = self.inner.lock().await;
        if !inner.enabled {
            return Ok(());
        }
        let progresstext = ProgressBar::create_progress_bar_text(percentage, extrainfo);
        if self.gated && self.position > 0 {
            // Record even while this video is not allowed to draw: its state
            // is shown the moment the message is handed over to it.
            inner
                .last_by_position
                .insert(self.position, (percentage, progresstext.clone()));
            if let Some(active) = &inner.active_position {
                if active.load(Ordering::Relaxed) != self.position {
                    return Ok(());
                }
            }
        }
        let now = Instant::now();

        // Check if an update is needed
        let should_update = if let Some(last) = inner.last_update {
            let time_passed = now.duration_since(last) >= MIN_UPDATE_INTERVAL;
            let significant_change = percentage.saturating_sub(inner.last_percentage) >= 5; // Minimum 5% change
            let is_completion = percentage == 100;

            time_passed && (significant_change || is_completion) || is_completion
        } else {
            true
        };

        if !should_update {
            return Ok(());
        }

        inner.last_update = Some(now);
        inner.last_percentage = percentage;

        if let Some(message_id) = inner.message_id {
            let result = inner
                .bot
                .edit_message_text(inner.chat_id, message_id, &progresstext)
                .await;

            match result {
                Ok(_) => {}
                Err(e) => {
                    let error_str = e.to_string();

                    // Reset on invalid ID
                    if error_str.contains("MESSAGE_ID_INVALID")
                        || error_str.contains("message to edit not found")
                        || error_str.contains("message can't be edited")
                    {
                        log::warn!("Progress message invalidated, creating new one");
                        inner.message_id = None;

                        // Create a new message only if not completed
                        if percentage < 100 {
                            if let Ok(msg) =
                                inner.bot.send_message(inner.chat_id, progresstext).await
                            {
                                inner.message_id = Some(msg.id);
                            }
                        }
                    } else if !error_str.contains("message is not modified") {
                        // Log only real errors
                        log::debug!("Progress update skipped: {}", e);
                    }
                }
            }
        } else {
            // Never create a message implicitly: only an explicit `start`
            // owns one. A stray progress callback must not produce a second
            // status bubble next to the batch's single one.
            log::debug!("Progress update before start ignored: {}%", percentage);
        }

        Ok(())
    }

    fn create_progress_bar_text(percentage: u8, extrainfo: Option<&str>) -> String {
        let bar_length = 20;
        let filled_length = ((percentage as f32 / 100.0) * bar_length as f32) as usize;

        let mut bar = String::new();
        bar.push('▓');
        for i in 0..bar_length {
            if i < filled_length {
                bar.push('█');
            } else {
                bar.push('░');
            }
        }
        bar.push('▓');

        let mut result = format!("🔄 Processing {}% {}", percentage, bar);
        if let Some(info) = extrainfo {
            result.push_str(&format!("\n{}", info));
        }
        result
    }

    pub async fn delete(&mut self) -> Result<(), anyhow::Error> {
        let mut inner = self.inner.lock().await;
        if !inner.enabled {
            return Ok(());
        }
        // A batch message outlives each individual video.
        if inner.keep_message {
            return Ok(());
        }
        self.remove_locked(&mut inner).await;
        Ok(())
    }

    /// Delete the batch's single status message after the last video.
    pub async fn delete_now(&mut self) -> Result<(), anyhow::Error> {
        let mut inner = self.inner.lock().await;
        if !inner.enabled {
            return Ok(());
        }
        self.remove_locked(&mut inner).await;
        Ok(())
    }

    async fn remove_locked(&self, inner: &mut ProgressBarInner) {
        if let Some(message_id) = inner.message_id.take() {
            if let Err(e) = inner.bot.delete_message(inner.chat_id, message_id).await {
                let error_str = e.to_string();
                if !error_str.contains("MESSAGE_ID_INVALID")
                    && !error_str.contains("message to delete not found")
                {
                    log::debug!("Failed to delete progress message: {}", e);
                }
            }
        }
    }
}
