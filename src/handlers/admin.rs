use std::env;
use teloxide::prelude::*;

pub async fn is_admin(msg: &Message) -> bool {
    let admin_ids_str = env::var("ADMIN_IDS").unwrap_or_default();
    let admin_ids: Vec<i64> = admin_ids_str
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    admin_ids.contains(&msg.chat.id.0)
}
