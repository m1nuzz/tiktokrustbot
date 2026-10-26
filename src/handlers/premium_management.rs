use std::collections::BTreeMap;
use std::sync::Arc;

use teloxide::dispatching::dialogue::{Dialogue, InMemStorage};
use teloxide::prelude::*;
use teloxide::types::{ChatId, KeyboardButton, KeyboardMarkup};

use crate::BotCtx;
use crate::database::DatabasePool;
use crate::handlers::admin::{is_admin, is_first_admin, resolve_telegram_ids};
use crate::handlers::admin_management::{can_access_premium_management, can_grant_premium};
use crate::handlers::broadcast::BroadcastState;
use crate::handlers::ui::{
    BTN_ADD_PREMIUM, BTN_ADMIN_PANEL, BTN_ADMIN_PREMIUM_GRANTS, BTN_CANCEL, BTN_PREMIUM_1D,
    BTN_PREMIUM_1H, BTN_PREMIUM_1Y, BTN_PREMIUM_7D, BTN_PREMIUM_30D, BTN_PREMIUM_90D,
    BTN_PREMIUM_CUSTOM, BTN_PREMIUM_LIST, BTN_PREMIUM_MANAGEMENT, BTN_REMOVE_PREMIUM,
};

type MyDialogue = Dialogue<BroadcastState, InMemStorage<BroadcastState>>;
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

impl DurationUnit {
    pub fn as_db_str(self) -> &'static str {
        match self {
            DurationUnit::Seconds => "seconds",
            DurationUnit::Minutes => "minutes",
            DurationUnit::Hours => "hours",
            DurationUnit::Days => "days",
            DurationUnit::Weeks => "weeks",
            DurationUnit::Months => "months",
            DurationUnit::Years => "years",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DurationUnit::Seconds => "second(s)",
            DurationUnit::Minutes => "minute(s)",
            DurationUnit::Hours => "hour(s)",
            DurationUnit::Days => "day(s)",
            DurationUnit::Weeks => "week(s)",
            DurationUnit::Months => "month(s)",
            DurationUnit::Years => "year(s)",
        }
    }

    /// Ten years, expressed in each unit.
    pub fn max_amount(self) -> i64 {
        match self {
            DurationUnit::Seconds => 315_360_000,
            DurationUnit::Minutes => 5_256_000,
            DurationUnit::Hours => 87_600,
            DurationUnit::Days => 3_650,
            DurationUnit::Weeks => 521,
            DurationUnit::Months => 120,
            DurationUnit::Years => 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationSpec {
    pub amount: i64,
    pub unit: DurationUnit,
}

impl DurationSpec {
    fn label(self) -> String {
        format!("{} {}", self.amount, self.unit.label())
    }
}

fn duration_unit_from_suffix(suffix: &str) -> Option<DurationUnit> {
    match suffix {
        "s" | "sec" | "secs" | "second" | "seconds" | "сек" | "секунда" | "секунды" | "секунд" => {
            Some(DurationUnit::Seconds)
        }
        "m" | "min" | "mins" | "minute" | "minutes" | "мин" | "минута" | "минуты" | "минут" => {
            Some(DurationUnit::Minutes)
        }
        "h" | "hr" | "hrs" | "hour" | "hours" | "ч" | "час" | "часа" | "часов" => {
            Some(DurationUnit::Hours)
        }
        "d" | "day" | "days" | "д" | "день" | "дня" | "дней" => {
            Some(DurationUnit::Days)
        }
        "w" | "week" | "weeks" | "нед" | "неделя" | "недели" | "недель" => {
            Some(DurationUnit::Weeks)
        }
        "mo" | "mon" | "month" | "months" | "мес" | "месяц" | "месяца" | "месяцев" => {
            Some(DurationUnit::Months)
        }
        "y" | "yr" | "yrs" | "year" | "years" | "г" | "год" | "года" | "лет" => {
            Some(DurationUnit::Years)
        }
        _ => None,
    }
}

/// Parse `30s`, `15m`, `12h`, `7d`, `2w`, `3mo`, `1y` and Russian words.
/// `m` is minutes, `mo` is months. Zero, negatives and unknown suffixes fail.
pub fn parse_duration(input: &str) -> Result<DurationSpec, String> {
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return Err("Send a duration like 30s, 15m, 12h, 7d, 2w, 3mo or 1y.".to_string());
    }

    let digits_len = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    let (digits, rest) = input.split_at(digits_len);
    if digits.is_empty() {
        return Err("Duration must start with a number, e.g. 30s or 1y.".to_string());
    }

    let amount = digits
        .parse::<i64>()
        .map_err(|_| "Duration number is too large.".to_string())?;
    if amount <= 0 {
        return Err("Duration must be greater than zero.".to_string());
    }

    let suffix = rest.trim();
    if suffix.is_empty() {
        return Err("Add a unit: s, m, h, d, w, mo or y.".to_string());
    }

    let unit = duration_unit_from_suffix(suffix)
        .ok_or_else(|| format!("Unknown duration unit: {}", suffix))?;
    if amount > unit.max_amount() {
        return Err("Maximum premium duration is 10 years.".to_string());
    }

    Ok(DurationSpec { amount, unit })
}

pub fn preset_duration(text: &str) -> Option<DurationSpec> {
    let (amount, unit) = match text {
        BTN_PREMIUM_1H => (1, DurationUnit::Hours),
        BTN_PREMIUM_1D => (1, DurationUnit::Days),
        BTN_PREMIUM_7D => (7, DurationUnit::Days),
        BTN_PREMIUM_30D => (30, DurationUnit::Days),
        BTN_PREMIUM_90D => (90, DurationUnit::Days),
        BTN_PREMIUM_1Y => (1, DurationUnit::Years),
        _ => return None,
    };
    Some(DurationSpec { amount, unit })
}

fn duration_keyboard() -> KeyboardMarkup {
    KeyboardMarkup::new(vec![
        vec![
            KeyboardButton::new(BTN_PREMIUM_1H),
            KeyboardButton::new(BTN_PREMIUM_1D),
        ],
        vec![
            KeyboardButton::new(BTN_PREMIUM_7D),
            KeyboardButton::new(BTN_PREMIUM_30D),
        ],
        vec![
            KeyboardButton::new(BTN_PREMIUM_90D),
            KeyboardButton::new(BTN_PREMIUM_1Y),
        ],
        vec![KeyboardButton::new(BTN_PREMIUM_CUSTOM)],
        vec![KeyboardButton::new(BTN_CANCEL)],
    ])
    .resize_keyboard()
}

fn user_id_from_msg(msg: &Message) -> i64 {
    msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0)
}

fn premium_management_keyboard(first_admin: bool) -> KeyboardMarkup {
    let mut rows = Vec::new();
    if first_admin {
        rows.push(vec![
            KeyboardButton::new(BTN_ADD_PREMIUM),
            KeyboardButton::new(BTN_REMOVE_PREMIUM),
        ]);
        rows.push(vec![
            KeyboardButton::new(BTN_PREMIUM_LIST),
            KeyboardButton::new(BTN_ADMIN_PREMIUM_GRANTS),
        ]);
    } else {
        rows.push(vec![KeyboardButton::new(BTN_ADD_PREMIUM)]);
    }
    rows.push(vec![KeyboardButton::new(BTN_ADMIN_PANEL)]);
    KeyboardMarkup::new(rows).resize_keyboard()
}

async fn send_premium_management(
    bot: &Bot,
    chat_id: ChatId,
    ctx: &BotCtx,
    db_pool: &DatabasePool,
    first_admin: bool,
) -> HandlerResult {
    let active = db_pool
        .get_premium_users(Some(ctx.bot_id.as_str()))
        .await
        .map(|users| users.len())
        .unwrap_or(0);
    let text = format!(
        "{} \nActive premium (this bot): {}",
        BTN_PREMIUM_MANAGEMENT, active
    );
    bot.send_message(chat_id, text)
        .reply_markup(premium_management_keyboard(first_admin))
        .await?;
    Ok(())
}

pub async fn premium_management_menu_handler(
    bot: Bot,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }

    let user_id = user_id_from_msg(&msg);
    if !can_access_premium_management(&db_pool, user_id).await {
        bot.send_message(
            msg.chat.id,
            "❌ Only the first admin can manage premium. Ask the first admin to allow grants.",
        )
        .await?;
        return Ok(());
    }

    let first_admin = is_first_admin(&db_pool, user_id).await;
    send_premium_management(&bot, msg.chat.id, &ctx, &db_pool, first_admin).await
}

pub async fn add_premium_prompt_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !can_grant_premium(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can grant premium.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        msg.chat.id,
        "Send one or more Telegram IDs or @usernames separated by spaces or commas.\n/cancel to abort.",
    )
    .await?;
    dialogue
        .update(BroadcastState::WaitingForPremiumUsers)
        .await?;
    Ok(())
}

pub async fn add_premium_users_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !can_grant_premium(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can grant premium.")
            .await?;
        dialogue.exit().await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    if text == "/cancel" {
        dialogue.exit().await?;
        return premium_management_menu_handler(bot, msg, _ctx, db_pool).await;
    }
    if text == BTN_ADMIN_PANEL {
        dialogue.exit().await?;
        return crate::handlers::admin_panel::admin_panel_text_handler(bot, msg, _ctx, db_pool)
            .await;
    }

    let user_ids = match resolve_telegram_ids(&bot, text).await {
        Ok(ids) => ids,
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {}\n\nSend again or /cancel.", e))
                .await?;
            return Ok(());
        }
    };

    dialogue
        .update(BroadcastState::WaitingForPremiumDuration {
            user_ids: user_ids.clone(),
        })
        .await?;
    bot.send_message(
        msg.chat.id,
        format!("Choose premium duration for {} user(s):", user_ids.len()),
    )
    .reply_markup(duration_keyboard())
    .await?;
    Ok(())
}

async fn grant_premium(
    bot: &Bot,
    chat_id: ChatId,
    ctx: &BotCtx,
    db_pool: &DatabasePool,
    admin_id: i64,
    user_ids: &[i64],
    spec: DurationSpec,
) -> HandlerResult {
    let granted = db_pool
        .set_users_premium_duration(
            ctx.bot_id.as_str(),
            user_ids,
            spec.amount,
            spec.unit.as_db_str(),
            Some(admin_id),
        )
        .await?;
    bot.send_message(
        chat_id,
        format!(
            "✅ Premium granted to {} user(s): {}.",
            granted,
            spec.label()
        ),
    )
    .await?;
    Ok(())
}

pub async fn premium_duration_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
    user_ids: Vec<i64>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        return Ok(());
    }
    let admin_id = user_id_from_msg(&msg);
    if !can_grant_premium(&db_pool, admin_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can grant premium.")
            .await?;
        dialogue.exit().await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    if text == "/cancel" {
        dialogue.exit().await?;
        return premium_management_menu_handler(bot, msg, ctx, db_pool).await;
    }
    if text == BTN_ADMIN_PANEL {
        dialogue.exit().await?;
        return crate::handlers::admin_panel::admin_panel_text_handler(bot, msg, ctx, db_pool)
            .await;
    }

    if text == BTN_PREMIUM_CUSTOM {
        dialogue
            .update(BroadcastState::WaitingForPremiumCustomDuration {
                user_ids: user_ids.clone(),
            })
            .await?;
        bot.send_message(
            msg.chat.id,
            "✍️ Свой срок — ввод текстом: 30s, 15m, 12h, 7d, 2w, 3mo, 1y\n\
             Можно словами: 30 секунд, 15 минут, 12 часов, 7 дней, 2 недели, 3 месяца, 1 год.",
        )
        .await?;
        return Ok(());
    }

    let Some(spec) = preset_duration(text) else {
        bot.send_message(msg.chat.id, "Choose a preset, ✍️ Свой срок, or ❌ Cancel.")
            .reply_markup(duration_keyboard())
            .await?;
        return Ok(());
    };

    grant_premium(&bot, msg.chat.id, &ctx, &db_pool, admin_id, &user_ids, spec).await?;
    dialogue.exit().await?;
    premium_management_menu_handler(bot, msg, ctx, db_pool).await
}

pub async fn premium_custom_duration_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
    user_ids: Vec<i64>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        return Ok(());
    }
    let admin_id = user_id_from_msg(&msg);
    if !can_grant_premium(&db_pool, admin_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can grant premium.")
            .await?;
        dialogue.exit().await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    if text == "/cancel" {
        dialogue.exit().await?;
        return premium_management_menu_handler(bot, msg, ctx, db_pool).await;
    }
    if text == BTN_ADMIN_PANEL {
        dialogue.exit().await?;
        return crate::handlers::admin_panel::admin_panel_text_handler(bot, msg, ctx, db_pool)
            .await;
    }

    let spec = match parse_duration(text) {
        Ok(spec) => spec,
        Err(e) => {
            bot.send_message(
                msg.chat.id,
                format!(
                    "❌ {}\n\n✍️ Свой срок — ввод текстом: 30s, 15m, 12h, 7d, 2w, 3mo, 1y\n\
                     Можно словами: 30 секунд, 15 минут, 12 часов, 7 дней, 2 недели, 3 месяца, 1 год.",
                    e
                ),
            )
            .await?;
            return Ok(());
        }
    };

    grant_premium(&bot, msg.chat.id, &ctx, &db_pool, admin_id, &user_ids, spec).await?;
    dialogue.exit().await?;
    premium_management_menu_handler(bot, msg, ctx, db_pool).await
}

pub async fn remove_premium_prompt_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can remove premium.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        msg.chat.id,
        "Send one or more Telegram IDs or @usernames to remove premium from, separated by spaces or commas.\n/cancel to abort.",
    )
    .await?;
    dialogue
        .update(BroadcastState::WaitingForRemovePremiumUsers)
        .await?;
    Ok(())
}

pub async fn remove_premium_users_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can remove premium.")
            .await?;
        dialogue.exit().await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    if text == "/cancel" {
        dialogue.exit().await?;
        return premium_management_menu_handler(bot, msg, ctx, db_pool).await;
    }
    if text == BTN_ADMIN_PANEL {
        dialogue.exit().await?;
        return crate::handlers::admin_panel::admin_panel_text_handler(bot, msg, ctx, db_pool)
            .await;
    }

    let user_ids = match resolve_telegram_ids(&bot, text).await {
        Ok(ids) => ids,
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {}\n\nSend again or /cancel.", e))
                .await?;
            return Ok(());
        }
    };

    let removed = db_pool
        .remove_users_premium(ctx.bot_id.as_str(), &user_ids)
        .await?;
    dialogue.exit().await?;
    bot.send_message(
        msg.chat.id,
        format!("✅ Premium removed from {} user(s).", removed),
    )
    .await?;
    premium_management_menu_handler(bot, msg, ctx, db_pool).await
}

pub async fn premium_list_handler(
    bot: Bot,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(
            msg.chat.id,
            "❌ Only the first admin can view premium users.",
        )
        .await?;
        return Ok(());
    }
    crate::handlers::admin_panel::premium_users_text_handler(bot, msg, ctx, db_pool).await
}

pub async fn admin_premium_grants_handler(
    bot: Bot,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can view this list.")
            .await?;
        return Ok(());
    }

    let (grants, total) = db_pool
        .get_admin_premium_grants(ctx.bot_id.as_str(), 50)
        .await?;
    if grants.is_empty() {
        bot.send_message(msg.chat.id, "👑 No active premium grants by admins.")
            .await?;
        return Ok(());
    }

    let mut grouped: BTreeMap<i64, Vec<(i64, String, String)>> = BTreeMap::new();
    for (admin_id, user_id, premium_until, granted_at) in grants.iter() {
        grouped.entry(*admin_id).or_default().push((
            *user_id,
            premium_until.clone(),
            granted_at.clone(),
        ));
    }

    let mut response = format!("👑 Granted by Admins — {} active\n", total);
    for (admin_id, users) in grouped {
        response.push_str(&format!("\nAdmin {}: {}\n", admin_id, users.len()));
        for (user_id, premium_until, granted_at) in users {
            response.push_str(&format!(
                "  {} | until {} | granted {}\n",
                user_id, premium_until, granted_at
            ));
        }
    }
    if total > grants.len() as i64 {
        response.push_str(&format!("\n… and {} more", total - grants.len() as i64));
    }

    bot.send_message(msg.chat.id, response).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_and_word_units() {
        assert_eq!(
            parse_duration("30s").unwrap(),
            DurationSpec {
                amount: 30,
                unit: DurationUnit::Seconds
            }
        );
        assert_eq!(
            parse_duration("15m").unwrap(),
            DurationSpec {
                amount: 15,
                unit: DurationUnit::Minutes
            }
        );
        assert_eq!(
            parse_duration("12h").unwrap(),
            DurationSpec {
                amount: 12,
                unit: DurationUnit::Hours
            }
        );
        assert_eq!(
            parse_duration("7d").unwrap(),
            DurationSpec {
                amount: 7,
                unit: DurationUnit::Days
            }
        );
        assert_eq!(
            parse_duration("2w").unwrap(),
            DurationSpec {
                amount: 2,
                unit: DurationUnit::Weeks
            }
        );
        assert_eq!(
            parse_duration("3mo").unwrap(),
            DurationSpec {
                amount: 3,
                unit: DurationUnit::Months
            }
        );
        assert_eq!(
            parse_duration("1y").unwrap(),
            DurationSpec {
                amount: 1,
                unit: DurationUnit::Years
            }
        );
        assert_eq!(
            parse_duration("30 секунд").unwrap(),
            DurationSpec {
                amount: 30,
                unit: DurationUnit::Seconds
            }
        );
        assert_eq!(
            parse_duration("2 недели").unwrap(),
            DurationSpec {
                amount: 2,
                unit: DurationUnit::Weeks
            }
        );
        assert_eq!(
            parse_duration("3 месяца").unwrap(),
            DurationSpec {
                amount: 3,
                unit: DurationUnit::Months
            }
        );
        assert_eq!(
            parse_duration("1 год").unwrap(),
            DurationSpec {
                amount: 1,
                unit: DurationUnit::Years
            }
        );
    }

    #[test]
    fn rejects_bad_and_over_cap_durations() {
        for bad in ["", "0d", "-1d", "abc", "10", "1 fortnight", "1.5d"] {
            assert!(parse_duration(bad).is_err(), "{} must fail", bad);
        }
        assert!(parse_duration("11y").is_err());
        assert!(parse_duration("121mo").is_err());
        assert!(parse_duration("3651d").is_err());
        assert!(parse_duration("315360001s").is_err());
    }

    #[test]
    fn preset_labels_map_to_expected_durations() {
        assert_eq!(
            preset_duration(BTN_PREMIUM_1H).unwrap(),
            DurationSpec {
                amount: 1,
                unit: DurationUnit::Hours
            }
        );
        assert_eq!(
            preset_duration(BTN_PREMIUM_1Y).unwrap(),
            DurationSpec {
                amount: 1,
                unit: DurationUnit::Years
            }
        );
        assert!(preset_duration(BTN_PREMIUM_CUSTOM).is_none());
    }
}
