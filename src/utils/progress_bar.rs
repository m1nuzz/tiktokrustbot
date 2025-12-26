use anyhow::Result;
use teloxide::{prelude::*, requests::Requester, types::{ChatId, MessageId}};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Instant, Duration};

const MIN_UPDATE_INTERVAL: Duration = Duration::from_secs(3);

struct ProgressBarInner {
    bot: Bot,
    chat_id: ChatId,
    message_id: Option<MessageId>,
    last_update: Option<Instant>,
    last_percentage: u8,
}

#[derive(Clone)]
pub struct ProgressBar {
    inner: Arc<Mutex<ProgressBarInner>>,
}

impl ProgressBar {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self::create_progressbar_static(bot, chat_id)
    }

    pub fn new_silent() -> Self {
        Self::create_progressbar_static(Bot::new("DUMMY_TOKEN"), ChatId(0))
    }

    fn create_progressbar_static(bot: Bot, chat_id: ChatId) -> Self {
        ProgressBar {
            inner: Arc::new(Mutex::new(ProgressBarInner {
                bot,
                chat_id,
                message_id: None,
                last_update: None,
                last_percentage: 0,
            })),
        }
    }

    pub async fn start(&mut self, initial_text: &str) -> Result<(), anyhow::Error> {
        let mut inner = self.inner.lock().await;
        let msg = inner.bot.send_message(inner.chat_id, initial_text).await?;
        inner.message_id = Some(msg.id);
        inner.last_update = Some(Instant::now());
        Ok(())
    }

    pub async fn update(&mut self, percentage: u8, extrainfo: Option<&str>) -> Result<(), anyhow::Error> {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();

        // Проверяем, нужно ли обновление
        let should_update = if let Some(last) = inner.last_update {
            let time_passed = now.duration_since(last) >= MIN_UPDATE_INTERVAL;
            let significant_change = percentage.saturating_sub(inner.last_percentage) >= 5; // Минимум 5% изменения
            let is_completion = percentage == 100;
            
            time_passed && (significant_change || is_completion) || is_completion
        } else {
            true
        };

        if !should_update {
            return Ok(())
        }

        inner.last_update = Some(now);
        inner.last_percentage = percentage;

        let progresstext = ProgressBar::create_progress_bar_text(percentage, extrainfo);

        if let Some(message_id) = inner.message_id {
            let result = inner
                .bot
                .edit_message_text(inner.chat_id, message_id, &progresstext)
                .await;

            match result {
                Ok(_) => {},
                Err(e) => {
                    let error_str = e.to_string();
                    
                    // Сброс при инвалидном ID
                    if error_str.contains("MESSAGE_ID_INVALID") 
                        || error_str.contains("message to edit not found")
                        || error_str.contains("message can't be edited") {
                        log::warn!("Progress message invalidated, creating new one");
                        inner.message_id = None;
                        
                        // Создать новое сообщение только если не завершено
                        if percentage < 100 {
                            if let Ok(msg) = inner.bot.send_message(inner.chat_id, progresstext).await {
                                inner.message_id = Some(msg.id);
                            }
                        }
                    } else if !error_str.contains("message is not modified") {
                        // Логируем только реальные ошибки
                        log::debug!("Progress update skipped: {}", e);
                    }
                }
            }
        } else {
            // Создать новое сообщение
            if let Ok(msg) = inner.bot.send_message(inner.chat_id, progresstext).await {
                inner.message_id = Some(msg.id);
            }
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
        if let Some(message_id) = inner.message_id {
            if let Err(e) = inner.bot.delete_message(inner.chat_id, message_id).await {
                let error_str = e.to_string();
                if !error_str.contains("MESSAGE_ID_INVALID") 
                    && !error_str.contains("message to delete not found") {
                    log::debug!("Failed to delete progress message: {}", e);
                }
            }
            inner.message_id = None;
        }
        Ok(())
    }
}