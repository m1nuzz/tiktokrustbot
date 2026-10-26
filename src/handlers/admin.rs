use std::collections::HashSet;
use std::env;
use std::sync::{OnceLock, RwLock};

use teloxide::prelude::*;
use teloxide::types::Recipient;

use crate::database::DatabasePool;

/// `None` means the cache has not been initialised yet, so the env fallback is
/// still authoritative. `Some(set)` means the DB is the source of truth.
static ADMIN_CACHE: OnceLock<RwLock<Option<HashSet<i64>>>> = OnceLock::new();

fn admin_cache() -> &'static RwLock<Option<HashSet<i64>>> {
    ADMIN_CACHE.get_or_init(|| RwLock::new(None))
}

pub async fn is_admin(msg: &Message) -> bool {
    if let Some(user) = msg.from.as_ref() {
        is_admin_id(user.id.0 as i64)
    } else {
        false
    }
}

/// Same admin check by raw id, for flows without a Message (web server).
pub fn is_admin_id(user_id: i64) -> bool {
    admin_ids().contains(&user_id)
}

/// Raw admin id list from ADMIN_IDS env (empty when unset).
pub fn env_admin_ids() -> Vec<i64> {
    let mut ids: Vec<i64> = env::var("ADMIN_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Admin ids from the in-memory DB cache, falling back to env before startup
/// initialises the cache. Sorted for stable display and stats SQL.
pub fn admin_ids() -> Vec<i64> {
    let cached = admin_cache()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();

    match cached {
        Some(set) => {
            let mut ids: Vec<i64> = set.into_iter().collect();
            ids.sort_unstable();
            ids
        }
        None => env_admin_ids(),
    }
}

/// Replace the cache with the DB-backed admin set. Empty is a valid value and
/// deliberately disables the env fallback once startup has run.
pub fn set_admin_cache(ids: impl IntoIterator<Item = i64>) {
    let set: HashSet<i64> = ids.into_iter().collect();
    *admin_cache()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(set);
}

pub async fn reload_admin_cache(db_pool: &DatabasePool) -> Result<(), anyhow::Error> {
    let ids = db_pool.list_admins().await?;
    set_admin_cache(ids);
    Ok(())
}

/// First admin = lowest `admins.id`. Only this id may add/remove admins or
/// change the premium-grant policy.
pub async fn is_first_admin(db_pool: &DatabasePool, user_id: i64) -> bool {
    matches!(db_pool.get_first_admin().await, Ok(Some(id)) if id == user_id)
}

/// Normalise one admin/premium target: numeric ids stay numeric, t.me links
/// and bare usernames become `@username`.
pub fn normalize_telegram_token(raw: &str) -> Option<String> {
    let token = raw.trim().trim_end_matches('/');
    if token.is_empty() {
        return None;
    }
    if token.parse::<i64>().is_ok() {
        return Some(token.to_string());
    }

    let without_scheme = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("http://"))
        .unwrap_or(token);

    let name = if let Some(rest) = without_scheme
        .strip_prefix("t.me/")
        .or_else(|| without_scheme.strip_prefix("telegram.me/"))
    {
        rest
    } else {
        without_scheme
    };

    let name = name.trim_start_matches('@').trim_matches('/');
    if name.is_empty() {
        None
    } else {
        Some(format!("@{}", name))
    }
}

pub fn parse_telegram_tokens(input: &str) -> Vec<String> {
    input
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter_map(normalize_telegram_token)
        .collect()
}

/// Resolve one numeric id or @username to a numeric Telegram user id.
///
/// Telegram's Bot API does not reliably resolve arbitrary private users by
/// username; `get_chat` succeeds only when the bot has seen that user. The
/// caller must therefore surface the error and ask for a numeric id.
pub async fn resolve_telegram_id(bot: &Bot, raw: &str) -> Result<i64, String> {
    let normalized = normalize_telegram_token(raw)
        .ok_or_else(|| format!("Invalid Telegram ID or username: {}", raw))?;

    if let Ok(id) = normalized.parse::<i64>() {
        if id > 0 {
            return Ok(id);
        }
        return Err(format!("Telegram ID must be positive: {}", raw));
    }

    match bot
        .get_chat(Recipient::ChannelUsername(normalized.clone()))
        .await
    {
        Ok(chat) => {
            let id = chat.id.0 as i64;
            if id > 0 {
                Ok(id)
            } else {
                Err(format!("{} is not a private user.", normalized))
            }
        }
        Err(e) => Err(format!(
            "Cannot resolve {}: {}. Ask the user to send /start or use a numeric ID.",
            normalized, e
        )),
    }
}

/// Resolve a whitespace/comma separated list, preserving input order and
/// dropping duplicates. Any bad token rejects the whole input.
pub async fn resolve_telegram_ids(bot: &Bot, input: &str) -> Result<Vec<i64>, String> {
    let tokens = parse_telegram_tokens(input);
    if tokens.is_empty() {
        return Err("No Telegram IDs or usernames provided.".to_string());
    }

    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for token in tokens {
        let id = resolve_telegram_id(bot, &token).await?;
        if seen.insert(id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn reset_admin_cache() {
        *admin_cache()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    // Helper function to test parsing logic without environment variables
    fn parse_admin_ids(admin_ids_str: &str) -> Vec<i64> {
        admin_ids_str
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect()
    }

    #[test]
    fn test_parse_admin_ids() {
        let admin_ids = parse_admin_ids("123456,789012, 345678");
        assert_eq!(admin_ids, vec![123456, 789012, 345678]);
    }

    #[test]
    fn test_parse_admin_ids_empty() {
        let admin_ids = parse_admin_ids("");
        assert_eq!(admin_ids, Vec::<i64>::new());
    }

    #[test]
    fn test_parse_admin_ids_with_spaces() {
        let admin_ids = parse_admin_ids(" 111111 , 222222 , 333333 ");
        assert_eq!(admin_ids, vec![111111, 222222, 333333]);
    }

    #[test]
    fn test_admin_id_matching() {
        let admin_ids = vec![123456i64, 789012i64];

        let admin_user_id: i64 = 123456;
        assert!(admin_ids.contains(&admin_user_id));

        let regular_user_id: i64 = 555555;
        assert!(!admin_ids.contains(&regular_user_id));
    }

    #[test]
    fn test_user_id_type_conversion() {
        let admin_ids = vec![123456i64, 789012i64];

        let telegram_user_id: u64 = 123456;
        let is_admin = admin_ids.contains(&(telegram_user_id as i64));
        assert!(is_admin);

        let regular_user_id: u64 = 555555;
        let is_admin = admin_ids.contains(&(regular_user_id as i64));
        assert!(!is_admin);
    }

    #[test]
    fn normalizes_numeric_and_username_tokens() {
        assert_eq!(normalize_telegram_token("12345"), Some("12345".to_string()));
        assert_eq!(
            normalize_telegram_token("@SomeUser"),
            Some("@SomeUser".to_string())
        );
        assert_eq!(
            normalize_telegram_token("SomeUser"),
            Some("@SomeUser".to_string())
        );
        assert_eq!(
            normalize_telegram_token("https://t.me/SomeUser/"),
            Some("@SomeUser".to_string())
        );
        assert_eq!(
            normalize_telegram_token("telegram.me/SomeUser"),
            Some("@SomeUser".to_string())
        );
        assert_eq!(normalize_telegram_token("   "), None);
    }

    #[test]
    fn splits_and_dedupes_input_tokens() {
        let tokens = parse_telegram_tokens("111, @two  t.me/three,111");
        assert_eq!(
            tokens,
            vec![
                "111".to_string(),
                "@two".to_string(),
                "@three".to_string(),
                "111".to_string()
            ]
        );
    }

    #[tokio::test]
    #[serial]
    async fn cache_falls_back_to_env_until_initialised() {
        unsafe {
            std::env::set_var("ADMIN_IDS", "3,1,2,1");
        }
        reset_admin_cache();
        assert_eq!(admin_ids(), vec![1, 2, 3]);

        set_admin_cache(vec![9]);
        assert_eq!(admin_ids(), vec![9]);

        reset_admin_cache();
        unsafe {
            std::env::remove_var("ADMIN_IDS");
        }
    }

    #[tokio::test]
    #[serial]
    async fn cache_reloads_from_db_and_first_admin_is_lowest_id() {
        let (pool, _file) = crate::database::setup_test_db().await;
        pool.add_admins(&[700, 500, 600]).await.unwrap();

        reload_admin_cache(&pool).await.unwrap();
        assert_eq!(admin_ids(), vec![500, 600, 700]);
        assert!(is_first_admin(&pool, 700).await);
        assert!(!is_first_admin(&pool, 500).await);

        reset_admin_cache();
    }
}
