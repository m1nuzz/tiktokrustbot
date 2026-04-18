use teloxide::prelude::*;
use teloxide::types::ChatMemberStatus;
use std::env;
use anyhow::Error;

pub async fn check_subscription(bot: &Bot, user_id: i64) -> Result<bool, Error> {
    let channel_ids_str = env::var("CHANNEL_IDS").unwrap_or_default();
    if channel_ids_str.is_empty() {
        return Ok(true);
    }

    let channel_ids = channel_ids_str.split(',');

    for channel_id in channel_ids {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            continue;
        }

        match bot.get_chat_member(channel_id.to_string(), UserId(user_id as u64)).await {
            Ok(member) => {
                let status = member.status();
                if !matches!(status, ChatMemberStatus::Member | ChatMemberStatus::Administrator | ChatMemberStatus::Owner) {
                    return Ok(false);
                }
            }
            Err(e) => {
                log::error!("Failed to get chat member for channel {}: {}", channel_id, e);
                return Err(e.into());
            }
        }
    }

    Ok(true)
}