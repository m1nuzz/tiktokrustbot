use anyhow::Result;
use teloxide::{prelude::*, requests::Requester, types::{ChatId, MessageId}};

#[derive(Clone)]
pub struct ProgressBar {
    bot: Bot,
    chat_id: ChatId,
    message_id: Option<MessageId>,
    last_update: Option<tokio::time::Instant>,
    last_percentage: u8,  // Добавить для отслеживания
}

impl ProgressBar {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self {
            bot,
            chat_id,
            message_id: None,
            last_update: None,
            last_percentage: 0,
        }
    }

    pub fn new_silent() -> Self {
        Self::new(Bot::new("DUMMY_TOKEN"), ChatId(0))
    }

    pub async fn start(&mut self, initial_text: &str) -> Result<(), anyhow::Error> {
        let msg = self.bot.send_message(self.chat_id, initial_text).await?;
        self.message_id = Some(msg.id);
        self.last_update = Some(tokio::time::Instant::now());
        Ok(())
    }

    pub async fn update(&mut self, percentage: u8, extrainfo: Option<&str>) -> Result<(), anyhow::Error> {
        // Увеличенный интервал - минимум 3 секунды между обновлениями
        const MIN_UPDATE_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(3);
        let now = tokio::time::Instant::now();

        // Проверяем, нужно ли обновление
        let should_update = if let Some(last) = self.last_update {
            let time_passed = now.duration_since(last) >= MIN_UPDATE_INTERVAL;
            let significant_change = percentage.saturating_sub(self.last_percentage) >= 5; // Минимум 5% изменения
            let is_completion = percentage == 100;
            
            time_passed && (significant_change || is_completion) || is_completion
        } else {
            true
        };

        if !should_update {
            return Ok(());
        }

        self.last_update = Some(now);
        self.last_percentage = percentage;

        if let Some(message_id) = self.message_id {
            let progresstext = self.create_progressbar(percentage, extrainfo);
            let result = self
                .bot
                .edit_message_text(self.chat_id, message_id, progresstext)
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
                        self.message_id = None;
                        
                        // Создать новое сообщение только если не завершено
                        if percentage < 100 {
                            if let Ok(msg) = self.bot.send_message(self.chat_id, self.create_progressbar(percentage, extrainfo)).await {
                                self.message_id = Some(msg.id);
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
            let progresstext = self.create_progressbar(percentage, extrainfo);
            if let Ok(msg) = self.bot.send_message(self.chat_id, progresstext).await {
                self.message_id = Some(msg.id);
            }
        }

        Ok(())
    }

    fn create_progressbar(&self, percentage: u8, extrainfo: Option<&str>) -> String {
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
        if let Some(message_id) = self.message_id {
            if let Err(e) = self.bot.delete_message(self.chat_id, message_id).await {
                let error_str = e.to_string();
                if !error_str.contains("MESSAGE_ID_INVALID") 
                    && !error_str.contains("message to delete not found") {
                    log::debug!("Failed to delete progress message: {}", e);
                }
            }
            self.message_id = None;
        }
        Ok(())
    }
}
