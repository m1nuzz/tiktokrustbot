use anyhow::Error;
use teloxide::prelude::*;
use teloxide::types::{ChatId, Recipient};

use crate::database::DatabasePool;

/// Whether the subscription gate applies to this actor.
///
/// Regular users follow the global `subscription_required` setting. Admins
/// follow the independent `admin_subscription_enabled` test switch, so the
/// flow can be tested without enabling the requirement for everyone.
pub async fn subscription_required_for(db_pool: &DatabasePool, is_user_admin: bool) -> bool {
    let key = if is_user_admin {
        "admin_subscription_enabled"
    } else {
        "subscription_required"
    };
    let default = !is_user_admin;
    db_pool
        .get_setting(key)
        .await
        .map(|value| value == "true")
        .unwrap_or(default)
}

/// Split admin input into channel tokens.
///
/// Accepts spaces, commas, `@username`, numeric IDs and `t.me` / `telegram.me`
/// links. Links are normalized to `@username`; numeric IDs stay untouched.
pub fn parse_channel_tokens(input: &str) -> Vec<String> {
    input
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter_map(normalize_channel_token)
        .collect()
}

fn normalize_channel_token(raw: &str) -> Option<String> {
    let token = raw.trim().trim_end_matches('/');
    if token.is_empty() {
        return None;
    }

    let without_scheme = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("http://"))
        .unwrap_or(token);

    for prefix in ["t.me/", "telegram.me/"] {
        if let Some(rest) = without_scheme.strip_prefix(prefix) {
            let name = rest.trim_start_matches('@').trim_matches('/');
            if name.is_empty() {
                return None;
            }
            return Some(format!("@{}", name));
        }
    }

    Some(token.to_string())
}

/// Build a teloxide recipient from a stored channel token.
pub fn channel_recipient(channel_id: &str) -> Recipient {
    let trimmed = channel_id.trim();
    if let Ok(id) = trimmed.parse::<i64>() {
        Recipient::Id(ChatId(id))
    } else if trimmed.starts_with('@') {
        Recipient::ChannelUsername(trimmed.to_string())
    } else {
        Recipient::ChannelUsername(format!("@{}", trimmed))
    }
}

/// Check that the user is present in every configured channel.
///
/// Returns `Ok(false)` only for a confirmed non-membership. Infrastructure or
/// API errors are logged and skipped (fail-open), because one broken channel
/// must not lock every user out of the bot.
pub async fn check_subscription(
    bot: &Bot,
    db_pool: &DatabasePool,
    user_id: i64,
    is_user_admin: bool,
) -> Result<bool, Error> {
    if !subscription_required_for(db_pool, is_user_admin).await {
        return Ok(true);
    }

    let channels = db_pool.list_channels().await?;
    if channels.is_empty() {
        log::warn!(
            "Subscription is required but no channels are configured; allowing user {}",
            user_id
        );
        return Ok(true);
    }

    let mut checked_any = false;
    for (channel_id, _name) in channels {
        let recipient = channel_recipient(&channel_id);
        match bot
            .get_chat_member(recipient, UserId(user_id as u64))
            .await
        {
            Ok(member) => {
                checked_any = true;
                if !member.kind.is_present() {
                    return Ok(false);
                }
            }
            Err(e) => {
                log::error!(
                    "Failed to check subscription for channel {}: {}",
                    channel_id,
                    e
                );
            }
        }
    }

    if !checked_any {
        log::warn!(
            "Could not check any subscription channel for user {}; allowing",
            user_id
        );
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::setup_test_db;

    #[test]
    fn parse_channel_tokens_supports_spaces_commas_and_links() {
        assert_eq!(
            parse_channel_tokens(" -100123, @chan  https://t.me/foo/ t.me/bar telegram.me/baz "),
            vec!["-100123", "@chan", "@foo", "@bar", "@baz"]
        );
        assert!(parse_channel_tokens(" ,, ").is_empty());
    }

    #[test]
    fn channel_recipient_parses_ids_and_usernames() {
        assert!(matches!(
            channel_recipient("-100123"),
            Recipient::Id(ChatId(-100123))
        ));
        assert!(matches!(
            channel_recipient("chan"),
            Recipient::ChannelUsername(ref username) if username.as_str() == "@chan"
        ));
        assert!(matches!(
            channel_recipient("@chan"),
            Recipient::ChannelUsername(ref username) if username.as_str() == "@chan"
        ));
    }

    #[tokio::test]
    async fn replace_channels_replaces_not_appends() {
        let (pool, _file) = setup_test_db().await;
        pool.replace_channels(vec![
            ("1".to_string(), "A".to_string()),
            ("2".to_string(), "B".to_string()),
        ])
        .await
        .unwrap();
        pool.replace_channels(vec![("3".to_string(), "C".to_string())])
            .await
            .unwrap();

        assert_eq!(
            pool.list_channels().await.unwrap(),
            vec![("3".to_string(), Some("C".to_string()))]
        );
    }

    #[tokio::test]
    async fn subscription_matrix_keeps_admin_test_independent() {
        let (pool, _file) = setup_test_db().await;

        assert!(subscription_required_for(&pool, false).await);
        assert!(!subscription_required_for(&pool, true).await);

        pool.set_setting("subscription_required", "false")
            .await
            .unwrap();
        pool.set_setting("admin_subscription_enabled", "true")
            .await
            .unwrap();
        assert!(!subscription_required_for(&pool, false).await);
        assert!(subscription_required_for(&pool, true).await);

        pool.set_setting("subscription_required", "true")
            .await
            .unwrap();
        pool.set_setting("admin_subscription_enabled", "false")
            .await
            .unwrap();
        assert!(subscription_required_for(&pool, false).await);
        assert!(!subscription_required_for(&pool, true).await);

        pool.set_setting("subscription_required", "false")
            .await
            .unwrap();
        pool.set_setting("admin_subscription_enabled", "false")
            .await
            .unwrap();
        assert!(!subscription_required_for(&pool, false).await);
        assert!(!subscription_required_for(&pool, true).await);
    }
}
