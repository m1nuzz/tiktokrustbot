use teloxide::prelude::*;
use teloxide::types::{KeyboardMarkup, KeyboardButton};
use teloxide::dispatching::dialogue::{InMemStorage, Dialogue};
use crate::handlers::admin::is_admin;
use crate::handlers::ui::{
    BTN_ADMIN_PANEL, BTN_SUBSCRIPTION, BTN_BACK,
    BTN_TOGGLE_ADS, BTN_TOGGLE_SUCCESS_NOTIFS, BTN_TOGGLE_FAIL_NOTIFS
};
use crate::database::DatabasePool;
use crate::handlers::broadcast::BroadcastState;
use std::sync::Arc;

pub const BTN_BROADCAST: &str = "📢 Broadcast";

type MyDialogue = Dialogue<BroadcastState, InMemStorage<BroadcastState>>;

pub async fn admin_panel_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    let ads_enabled = db_pool.get_setting("ads_enabled").await.map(|v| v == "true").unwrap_or(true);
    let admin_ads_enabled = db_pool.get_setting("admin_ads_enabled").await.map(|v| v == "true").unwrap_or(false);
    let notify_success = db_pool.get_setting("notify_success").await.map(|v| v == "true").unwrap_or(true);
    let notify_fail = db_pool.get_setting("notify_fail").await.map(|v| v == "true").unwrap_or(true);

    let keyboard = KeyboardMarkup::new(vec![
        vec![KeyboardButton::new("📊 Stats"), KeyboardButton::new("📈 Daily Stats")],
        vec![KeyboardButton::new("📅 Week"), KeyboardButton::new("🔻 Funnel")],
        vec![KeyboardButton::new(BTN_BROADCAST), KeyboardButton::new("➕ Add Premium User")],
        vec![KeyboardButton::new("🏆 Top 10"), KeyboardButton::new("👥 All users")],
        vec![KeyboardButton::new("💎 Premium Users")],
        vec![KeyboardButton::new(BTN_SUBSCRIPTION)],
        vec![
            KeyboardButton::new(format!("{}{}", BTN_TOGGLE_ADS, if ads_enabled { "ON ✅" } else { "OFF ❌" })),
            KeyboardButton::new(format!("🔔 Admin Ads: {}", if admin_ads_enabled { "ON ✅" } else { "OFF ❌" })),
        ],
        vec![
            KeyboardButton::new(format!("{}{}", BTN_TOGGLE_SUCCESS_NOTIFS, if notify_success { "ON ✅" } else { "OFF ❌" })),
            KeyboardButton::new(format!("{}{}", BTN_TOGGLE_FAIL_NOTIFS, if notify_fail { "ON ✅" } else { "OFF ❌" })),
        ],
        vec![KeyboardButton::new(BTN_BACK)],
    ])
    .resize_keyboard();

    bot.send_message(msg.chat.id, BTN_ADMIN_PANEL)
        .reply_markup(keyboard)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    Ok(())
}

pub async fn add_premium_user_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        return Ok(());
    }

    if let Some(text) = msg.text() {
        if text == "/cancel" {
            bot.send_message(msg.chat.id, "❌ Cancelled.")
                .reply_markup(crate::handlers::command::get_main_reply_keyboard())
                .await?;
            dialogue.exit().await?;
            return Ok(());
        }

        match text.parse::<i64>() {
            Ok(user_id) => {
                match db_pool.set_user_premium(crate::database::PRIMARY_BOT_ID, user_id, 30).await {
                    Ok(_) => {
                        bot.send_message(msg.chat.id, format!("✅ User {} granted 30 days of Premium!", user_id))
                            .reply_markup(crate::handlers::command::get_main_reply_keyboard())
                            .await?;
                        dialogue.exit().await?;
                    }
                    Err(e) => {
                        log::error!("Failed to add premium manually: {}", e);
                        bot.send_message(msg.chat.id, "❌ Database error.")
                            .await?;
                    }
                }
            }
            Err(_) => {
                bot.send_message(msg.chat.id, "⚠️ Please send a valid numeric Telegram ID (or /cancel):")
                    .await?;
            }
        }
    }

    Ok(())
}

/// Escape special characters for Telegram MarkdownV2
pub fn escape_markdown_v2(s: &str) -> String {
    s.replace("_", "\\_")
     .replace("*", "\\*")
     .replace("[", "\\[")
     .replace("]", "\\]")
     .replace("(", "\\(")
     .replace(")", "\\)")
     .replace("~", "\\~")
     .replace("`", "\\`")
     .replace(">", "\\>")
     .replace("#", "\\#")
     .replace("+", "\\+")
     .replace("-", "\\-")
     .replace("=", "\\=")
     .replace("|", "\\|")
     .replace("{", "\\{")
     .replace("}", "\\}")
     .replace(".", "\\.")
     .replace("!", "\\!")
}

pub async fn daily_stats_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        return Ok(());
    }

    match db_pool.get_rich_daily_stats(&[]).await {
        Ok(all) => {
            // Same numbers without admin (test/self) traffic.
            let admins = crate::handlers::admin::admin_ids();
            let s = db_pool
                .get_rich_daily_stats(&admins)
                .await
                .unwrap_or_else(|_| all.clone());
            let user_conv = if s.unique_users > 0 { (s.unique_downloaders as f64 / s.unique_users as f64) * 100.0 } else { 0.0 };
            let inv_pay_cr = if s.invoices_sent > 0 { (s.payments_count as f64 / s.invoices_sent as f64) * 100.0 } else { 0.0 };
            let (pb_valued, pb_free) = db_pool.get_postback_stats(1).await.unwrap_or((0, 0));
            let (pb_valued_w, pb_free_w) = db_pool.get_postback_stats(7).await.unwrap_or((0, 0));
            let pb_total = pb_valued + pb_free;
            let uncredited_alert =
                pb_total >= 10 && (pb_free as f64 / pb_total as f64) > 0.8;

            // Helper to escape anything
            let e = |s: String| escape_markdown_v2(&s);

            let mut response = format!(
                "📊 *Daily Report — {}*\n\
                📌 *С админом:* 👥 {} · ⬇️ {} · 📦 {}\n\n\
                *Activity Today \\(без админа\\)*\n\
                👥 Unique Users:       {} \\({}{} vs yesterday\\)\n\
                ⬇️ Unique Downloaders: {} \\({}% of users\\)\n\
                📦 Total Downloads:    {}\n\
                👁 Ad Impressions:     {}\n\
                🆕 New Users Today:    {}\n\
                🔁 Returning Users:    {}\n\n\
                *Monetization \\(Stars Premium\\)*\n\
                💰 Payments Today:     {}\n\
                ⭐ Revenue \\(Stars\\):    {}\n\
                🔄 Invoices Sent:      {}\n\
                💳 Invoice → Pay CR:   {}%\n\n\
                *📢 Ads \\(Monetag\\)*\n\
                💰 Засчитано сегодня:  {}\n\
                🆓 Бесплатных:         {}\n\
                📊 За 7 дней:          {} / {}\n",
                e(s.date),
                e(all.unique_users.to_string()),
                e(all.unique_downloaders.to_string()),
                e(all.total_downloads.to_string()),
                e(s.unique_users.to_string()), 
                if s.unique_users_delta >= 0 { "\\+" } else { "" }, 
                e(s.unique_users_delta.to_string()),
                e(s.unique_downloaders.to_string()), 
                e(format!("{:.1}", user_conv)),
                e(s.total_downloads.to_string()),
                e(s.ad_impressions.to_string()),
                e(s.new_users.to_string()),
                e(s.returning_users.to_string()),
                e(s.payments_count.to_string()),
                e(s.revenue_xtr.to_string()),
                e(s.invoices_sent.to_string()),
                e(format!("{:.1}", inv_pay_cr)),
                e(pb_valued.to_string()),
                e(pb_free.to_string()),
                e(pb_valued_w.to_string()),
                e(pb_free_w.to_string())
            );
            if uncredited_alert {
                response.push_str("⚠️ *Незачтено больше 80% — проверь постбэк\\-URL в SSP, секрет и саппорт Monetag*\n");
            }

            if let Some((hour, count)) = s.peak_hour {
                response.push_str(&format!(
                    "🕐 *Peak Hour:* {:02}:00–{:02}:00 \\({} downloads\\)\n\n", 
                    hour, hour + 1, e(count.to_string())
                ));
            }

            response.push_str("🏆 *Top 10 Downloaders Today:*\n");
            for (index, (user, count)) in s.top_downloaders.iter().enumerate() {
                response.push_str(&format!(
                    "{}\\. `{}` — {} downloads\n", 
                    index + 1, e(user.to_string()), e(count.to_string())
                ));
            }
            if s.top_downloaders.is_empty() { response.push_str("No activity yet\\.\n"); }

            response.push_str("\n🕓 *Last Active Today:*\n");
            for (index, (user, time)) in s.last_active_users.iter().enumerate() {
                response.push_str(&format!(
                    "{}\\. `{}` — {}\n", 
                    index + 1, e(user.to_string()), e(time.clone())
                ));
            }

            bot.send_message(msg.chat.id, response)
                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                .await?;
        }
        Err(e) => {
            log::error!("Daily stats error: {}", e);
            bot.send_message(msg.chat.id, "❌ Error retrieving daily stats.").await?;
        }
    }
    Ok(())
}

pub async fn weekly_stats_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        return Ok(());
    }

    let admins = crate::handlers::admin::admin_ids();
    let days_all = db_pool.get_weekly_stats(7, &[]).await;
    match db_pool.get_weekly_stats(7, &admins).await {
        Ok(days) => {
            let e = |s: String| escape_markdown_v2(&s);
            let mut response = String::from("📅 *Weekly Report — last 7 days \\(без админа\\)*\n\n");
            let (mut total_users, mut total_new, mut total_dl, mut total_blocks) = (0i64, 0i64, 0i64, 0i64);
            for d in &days {
                total_users += d.unique_users;
                total_new += d.new_users;
                total_dl += d.downloads;
                total_blocks += d.blocks;
                response.push_str(&format!(
                    "{} 👥 {} \\(\\+{} new\\) 📦 {} 🚫 {}\n",
                    e(d.date.clone()),
                    e(d.unique_users.to_string()),
                    e(d.new_users.to_string()),
                    e(d.downloads.to_string()),
                    e(d.blocks.to_string())
                ));
            }
            response.push_str(&format!(
                "\n*Total \\(sum of days\\):* 👥 {} 🆕 {} 📦 {} 🚫 {}\n",
                e(total_users.to_string()),
                e(total_new.to_string()),
                e(total_dl.to_string()),
                e(total_blocks.to_string())
            ));
            let (pb_valued_w, pb_free_w) = db_pool.get_postback_stats(7).await.unwrap_or((0, 0));
            response.push_str(&format!(
                "📢 *Ads 7d:* 💰 {} · 🆓 {}\n",
                e(pb_valued_w.to_string()),
                e(pb_free_w.to_string())
            ));
            if let Ok(all_days) = days_all {
                let (mut au, mut an, mut ad, mut ab) = (0i64, 0i64, 0i64, 0i64);
                for d in &all_days {
                    au += d.unique_users;
                    an += d.new_users;
                    ad += d.downloads;
                    ab += d.blocks;
                }
                response.push_str(&format!(
                    "📌 *С админом \\(Σ\\):* 👥 {} 🆕 {} 📦 {} 🚫 {}\n",
                    e(au.to_string()),
                    e(an.to_string()),
                    e(ad.to_string()),
                    e(ab.to_string())
                ));
            }
            match db_pool.get_ref_stats(7).await {
                Ok(refs) if !refs.is_empty() => {
                    response.push_str("\n🔗 *Top refs \\(new users, 7d\\):*\n");
                    for (ref_code, count) in &refs {
                        response.push_str(&format!(
                            "{} — {}\n",
                            e(ref_code.clone().unwrap_or_else(|| "(direct)".to_string())),
                            e(count.to_string())
                        ));
                    }
                }
                _ => {}
            }
            bot.send_message(msg.chat.id, response)
                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                .await?;
        }
        Err(e) => {
            log::error!("Weekly stats error: {}", e);
            bot.send_message(msg.chat.id, "❌ Error retrieving weekly stats.").await?;
        }
    }
    Ok(())
}

fn funnel_conv(step: i64, prev: i64) -> String {
    if prev > 0 {
        format!("{:.1}%", step as f64 / prev as f64 * 100.0)
    } else {
        "—".to_string()
    }
}

pub async fn funnel_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        return Ok(());
    }

    // Funnel diagnoses real users: admin (test/self) traffic excluded.
    let admins = crate::handlers::admin::admin_ids();
    match db_pool.get_funnel_stats(7, &admins).await {
        Ok(days) => {
            let e = |s: String| escape_markdown_v2(&s);
            let mut response = String::from("🔻 *Conversion Funnel — last 7 days \\(без админа\\)*\n\n");
            for d in &days {
                response.push_str(&format!(
                    "*{}*\nS0 start {} → S1 link {} \\({}\\) → S2 ad {} \\({}\\) → S3 claim {} \\({}\\) → S4 got video {} \\({}\\) \\(💰 {} · 🆓 {}\\)\n💀 expired {} · failed {}\n\n",
                    e(d.date.clone()),
                    e(d.started.to_string()),
                    e(d.link_sent.to_string()), e(funnel_conv(d.link_sent, d.started)),
                    e(d.ad_watched.to_string()), e(funnel_conv(d.ad_watched, d.link_sent)),
                    e(d.claimed.to_string()), e(funnel_conv(d.claimed, d.ad_watched)),
                    e(d.delivered.to_string()), e(funnel_conv(d.delivered, d.claimed)),
                    e(d.delivered_paid.to_string()),
                    e(d.delivered_free.to_string()),
                    e(d.expired.to_string()),
                    e(d.failed.to_string()),
                ));
            }
            response.push_str("S0\\=start · S1\\=sent link · S2\\=watched ad · S3\\=claimed · S4\\=delivered \\(💰\\=valued, 🆓\\=таймер/без зачёта\\)\\. Biggest drop \\= fix first\\.\n");
            bot.send_message(msg.chat.id, response)
                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                .await?;
        }
        Err(e) => {
            log::error!("Funnel stats error: {}", e);
            bot.send_message(msg.chat.id, "❌ Error retrieving funnel stats.").await?;
        }
    }
    Ok(())
}

pub async fn admin_ads_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        return Ok(());
    }

    let curr = db_pool.get_setting("admin_ads_enabled").await.map(|v| v == "true").unwrap_or(false);
    let next = if !curr { "true" } else { "false" };
    db_pool.set_setting("admin_ads_enabled", next).await?;
    
    admin_panel_text_handler(bot, msg, db_pool).await
}

pub async fn premium_users_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    let result = db_pool.get_premium_users().await;

    match result {
        Ok(users) => {
            let mut response = format!("💎 Premium Users - Total: {}\n\n", users.len());
            for (user_id, premium_until, last_active) in users.iter() {
                response.push_str(&format!(
                    "👤 User: {} | 📅 Until: {} | 🕒 Last active: {}\n",
                    user_id, premium_until, last_active
                ));
            }
            if users.is_empty() {
                response.push_str("No active premium users found.");
            }
            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("Premium users DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve premium users list.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

// New handlers
pub async fn stats_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    let result = db_pool.execute_with_timeout(|conn| {
        let total_users: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        let total_downloads: i64 = conn.query_row("SELECT COUNT(*) FROM downloads", [], |row| row.get(0))?;
        Ok((total_users, total_downloads))
    }).await;

    match result {
        Ok((total_users, total_downloads)) => {
            let response = format!(
                "📊 Statistics\n\n\
                 👥 Total users: {}\n\
                 📥 Total downloads: {}",
                total_users, total_downloads
            );
            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("Stats DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve statistics.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

pub async fn top10_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    let result = db_pool.execute_with_timeout(|conn| {
        let mut stmt = conn.prepare(
            "SELECT user_telegram_id, COUNT(*) as count
             FROM downloads
             GROUP BY user_telegram_id
             ORDER BY count DESC
             LIMIT 10"
        )?;

        let users_iter = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut users = Vec::new();
        for user_result in users_iter {
            users.push(user_result?);
        }
        Ok(users)
    }).await;

    match result {
        Ok(users) => {
            let mut response = String::from("🏆 Top 10 Users\n\n");
            for (index, (user_id, count)) in users.iter().enumerate() {
                response.push_str(&format!("{}. User {} - {} downloads\n", index + 1, user_id, count));
            }

            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("Top 10 DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve top users.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

pub async fn all_users_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    // SQL query with LEFT JOIN and COUNT
    let result = db_pool.execute_with_timeout(|conn| {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;

        let mut stmt = conn.prepare(
            "SELECT u.telegram_id, u.last_active, COUNT(d.id) as download_count
             FROM users u
             LEFT JOIN downloads d ON u.telegram_id = d.user_telegram_id
             GROUP BY u.telegram_id, u.last_active
             ORDER BY download_count DESC
             LIMIT 50"
        )?;

        let users_iter = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,      // telegram_id
                row.get::<_, String>(1)?,   // last_active
                row.get::<_, i64>(2)?       // download_count
            ))
        })?;

        let mut users = Vec::new();
        for user_result in users_iter {
            users.push(user_result?);
        }
        Ok((count, users))
    }).await;

    match result {
        Ok((total_count, users)) => {
            let mut response = format!("📊 All Users - Total: {} (last 50)\n\n", total_count);
            for (user_id, last_active, downloads) in users.iter() {
                response.push_str(&format!(
                    "👤 User: {} | 📥 Downloads: {} | 🕒 {}\n",
                    user_id, downloads, last_active
                ));
            }
            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("All users DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve users list.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_markdown_v2() {
        let input = "Hello (world) + [test] - 1.2! _ * ~ ` > # = | { }";
        let expected = "Hello \\(world\\) \\+ \\[test\\] \\- 1\\.2\\! \\_ \\* \\~ \\` \\> \\# \\= \\| \\{ \\}";
        assert_eq!(escape_markdown_v2(input), expected);
    }
}
