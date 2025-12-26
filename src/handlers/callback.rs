use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardMarkup, InlineKeyboardButton};
use rusqlite::params;
use std::sync::Arc;
use std::env;

use crate::database::DatabasePool;
use crate::handlers::admin::is_admin;
use crate::handlers::ui::{BTN_ADMIN_PANEL, BTN_SETTINGS, BTN_FORMAT, BTN_SUBSCRIPTION, BTN_BACK};

const USERS_PER_PAGE: i64 = 10;

fn get_admin_ids() -> Vec<i64> {
    env::var("ADMIN_IDS").unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

pub async fn callback_handler(bot: Bot, q: CallbackQuery, db_pool: Arc<DatabasePool>) -> Result<(), anyhow::Error> {
    if let Some(data) = q.data {
        log::info!("Received callback query with data: {}", data);

        if let Some(ref maybe_message) = q.message {
            if let Some(message) = maybe_message.regular_message() {
                if data.starts_with("set_quality_") {
                    let quality = data.split_at("set_quality_".len()).1;
                    let user_id = message.chat.id.0;
                    let quality_string = quality.to_string();
                    
                    let result = db_pool.execute_with_timeout(move |conn| {
                        conn.execute(
                            "UPDATE users SET quality_preference = ?1 WHERE telegram_id = ?2",
                            params![quality_string, user_id],
                        )
                    }).await;
                    
                    match result {
                        Ok(_) => {
                            db_pool.invalidate_user_quality_cache(user_id).await;
                            bot.answer_callback_query(q.id).text(&format!("Quality set to {}", quality)).await?;
                        },
                        Err(e) => {
                            log::error!("Failed to update quality preference: {}", e);
                            bot.answer_callback_query(q.id).text("Failed to update quality preference").await?;
                        }
                    }
                } else if data.starts_with("admin_stats_") {
                    if !is_admin(&message).await {
                        bot.answer_callback_query(q.id).text("Access denied.").await?;
                        return Ok(());
                    }

                    let period = data.split_at("admin_stats_".len()).1;
                    let (time_filter, title_period) = match period {
                        "24h" => ("AND downloaddate >= datetime('now', '-1 day')", "last 24 hours"),
                        "7d" => ("AND downloaddate >= datetime('now', '-7 days')", "last 7 days"),
                        "30d" => ("AND downloaddate >= datetime('now', '-30 days')", "last 30 days"),
                        "all" => ("", "all time"),
                        _ => return Ok(()),
                    };

                    let admin_ids = get_admin_ids();
                    let admin_ids_params = admin_ids.iter().map(|&id| id.to_string()).collect::<Vec<String>>().join(",");

                    let query = format!(
                        "SELECT COUNT(*), COUNT(DISTINCT usertelegramid) FROM downloads WHERE usertelegramid NOT IN ({}) {}",
                        admin_ids_params, time_filter
                    );
                    
                    let stats = db_pool.execute_with_timeout(move |conn| {
                         conn.query_row(&query, [], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                        })
                    }).await;

                    match stats {
                        Ok((total_downloads, unique_users)) => {
                            let text = format!("Statistics for {}:\n\nTotal Downloads: {}\nUnique Users: {}", title_period, total_downloads, unique_users);
                            let keyboard = InlineKeyboardMarkup::new(vec![
                                vec![InlineKeyboardButton::callback("Back", "admin_stats")],
                            ]);
                            bot.edit_message_text(message.chat.id, message.id, text).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        },
                        Err(e) => {
                            log::error!("Failed to get stats: {}", e);
                            bot.answer_callback_query(q.id).text("Failed to retrieve statistics.").await?;
                        }
                    }
                } else if data.starts_with("admin_top10_") {
                    if !is_admin(&message).await {
                        bot.answer_callback_query(q.id).text("Access denied.").await?;
                        return Ok(());
                    }

                    let period = data.split_at("admin_top10_".len()).1;
                    let (time_filter, title_period) = match period {
                        "24h" => ("WHERE downloaddate >= datetime('now', '-1 day')", "last 24 hours"),
                        "7d" => ("WHERE downloaddate >= datetime('now', '-7 days')", "last 7 days"),
                        "30d" => ("WHERE downloaddate >= datetime('now', '-30 days')", "last 30 days"),
                        "all" => ("", "all time"),
                        _ => return Ok(()),
                    };

                    let admin_ids = get_admin_ids();
                    let admin_ids_params = admin_ids.iter().map(|id| id.to_string()).collect::<Vec<String>>().join(",");
                    
                    let mut where_clause = time_filter.to_string();
                    if !admin_ids.is_empty() {
                        if where_clause.is_empty() {
                            where_clause.push_str("WHERE ");
                        } else {
                            where_clause.push_str(" AND ");
                        }
                        where_clause.push_str(&format!("usertelegramid NOT IN ({})", admin_ids_params));
                    }


                    let query = format!(
                        "SELECT usertelegramid, COUNT(*) AS cnt FROM downloads {} GROUP BY usertelegramid ORDER BY cnt DESC LIMIT 10",
                        where_clause
                    );

                    let top_users = db_pool.execute_with_timeout(move |conn| {
                        let mut stmt = conn.prepare(&query)?;
                        let users_iter = stmt.query_map([], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                        })?;
                        let mut users = Vec::new();
                        for user in users_iter {
                            users.push(user?);
                        }
                        Ok(users)
                    }).await;
                    
                    match top_users {
                        Ok(users) => {
                            let mut text = format!("Top 10 users for {}:\n\n", title_period);
                            for (i, (user_id, count)) in users.iter().enumerate() {
                                text.push_str(&format!("{}. {} - {} downloads\n", i + 1, user_id, count));
                            }
                            let keyboard = InlineKeyboardMarkup::new(vec![
                                vec![InlineKeyboardButton::callback("Back", "admin_top10")],
                            ]);
                            bot.edit_message_text(message.chat.id, message.id, text).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        },
                        Err(e) => {
                            log::error!("Failed to get top 10 users: {}", e);
                            bot.answer_callback_query(q.id).text("Failed to retrieve top 10 users.").await?;
                        }
                    }
                } else if data.starts_with("admin_users_page_") {
                    if !is_admin(&message).await {
                        bot.answer_callback_query(q.id).text("Access denied.").await?;
                        return Ok(());
                    }

                    let offset: i64 = data.split_at("admin_users_page_".len()).1.parse().unwrap_or(0);

                    let users_data = db_pool.execute_with_timeout(move |conn| {
                        let mut stmt = conn.prepare(
                            "SELECT u.telegramid, u.lastactive, COUNT(d.id) AS downloads_cnt
                             FROM users u
                             LEFT JOIN downloads d ON d.usertelegramid = u.telegramid
                             GROUP BY u.telegramid
                             ORDER BY downloads_cnt DESC, u.lastactive DESC
                             LIMIT ? OFFSET ?;"
                        )?;
                        let users_iter = stmt.query_map(params![USERS_PER_PAGE, offset], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
                        })?;
                        
                        let mut users = Vec::new();
                        for user_result in users_iter {
                            users.push(user_result?);
                        }
                        Ok(users)
                    }).await;

                    let total_users: i64 = db_pool.execute_with_timeout(|conn| {
                        conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
                    }).await.unwrap_or(0);


                    match users_data {
                        Ok(users) => {
                            let mut text = String::from("All Users:\n\n");
                            for (id, last_active, count) in users {
                                text.push_str(&format!("ID: {}, Last Active: {}, Downloads: {}\n", id, last_active, count));
                            }

                            let mut keyboard_rows = Vec::new();
                            let mut nav_buttons = Vec::new();

                            if offset > 0 {
                                nav_buttons.push(InlineKeyboardButton::callback("⬅️ Prev", format!("admin_users_page_{}", offset - USERS_PER_PAGE)));
                            }
                            if offset + USERS_PER_PAGE < total_users {
                                nav_buttons.push(InlineKeyboardButton::callback("Next ➡️", format!("admin_users_page_{}", offset + USERS_PER_PAGE)));
                            }
                            
                            if !nav_buttons.is_empty() {
                                keyboard_rows.push(nav_buttons);
                            }
                            keyboard_rows.push(vec![InlineKeyboardButton::callback("Back", "back_to_admin_panel")]);
                            
                            let keyboard = InlineKeyboardMarkup::new(keyboard_rows);
                            bot.edit_message_text(message.chat.id, message.id, text).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        },
                        Err(e) => {
                            log::error!("Failed to get users: {}", e);
                            bot.answer_callback_query(q.id).text("Failed to retrieve users.").await?;
                        }
                    }

                }
                else {
                    match data.as_str() {
                        "settings" => {
                            let mut keyboard_rows = vec![vec![
                                InlineKeyboardButton::callback(BTN_FORMAT, "format_menu"),
                            ]];

                            if is_admin(&message).await {
                                keyboard_rows.push(vec![
                                    InlineKeyboardButton::callback(BTN_ADMIN_PANEL, "admin_panel"),
                                ]);
                            }

                            keyboard_rows.push(vec![
                                InlineKeyboardButton::callback(BTN_BACK, "back_to_main"),
                            ]);

                            let keyboard = InlineKeyboardMarkup::new(keyboard_rows);

                            bot.edit_message_text(message.chat.id, message.id, BTN_SETTINGS).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        "admin_panel" => {
                            if !is_admin(&message).await {
                                bot.answer_callback_query(q.id).text("Access denied.").await?;
                                return Ok(());
                            }
                            let keyboard = InlineKeyboardMarkup::new(vec![
                                vec![InlineKeyboardButton::callback("Stats", "admin_stats")],
                                vec![InlineKeyboardButton::callback("Top 10", "admin_top10")],
                                vec![InlineKeyboardButton::callback("All users", "admin_users_page_0")],
                                vec![InlineKeyboardButton::callback(BTN_SUBSCRIPTION, "subscription_menu")],
                                vec![InlineKeyboardButton::callback(BTN_BACK, "back_to_settings")],
                            ]);
                            bot.edit_message_text(message.chat.id, message.id, BTN_ADMIN_PANEL).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        "admin_stats" => {
                            if !is_admin(&message).await {
                                bot.answer_callback_query(q.id).text("Access denied.").await?;
                                return Ok(());
                            }
                             let keyboard = InlineKeyboardMarkup::new(vec![
                                vec![
                                    InlineKeyboardButton::callback("24h", "admin_stats_24h"),
                                    InlineKeyboardButton::callback("7d", "admin_stats_7d"),
                                ],
                                vec![
                                    InlineKeyboardButton::callback("30d", "admin_stats_30d"),
                                    InlineKeyboardButton::callback("All time", "admin_stats_all"),
                                ],
                                vec![InlineKeyboardButton::callback(BTN_BACK, "back_to_admin_panel")],
                            ]);

                            bot.edit_message_text(message.chat.id, message.id, "Select statistics period:").await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        "admin_top10" => {
                            if !is_admin(&message).await {
                                bot.answer_callback_query(q.id).text("Access denied.").await?;
                                return Ok(());
                            }
                            let keyboard = InlineKeyboardMarkup::new(vec![
                                vec![
                                    InlineKeyboardButton::callback("24h", "admin_top10_24h"),
                                    InlineKeyboardButton::callback("7d", "admin_top10_7d"),
                                ],
                                vec![
                                    InlineKeyboardButton::callback("30d", "admin_top10_30d"),
                                    InlineKeyboardButton::callback("All time", "admin_top10_all"),
                                ],
                                vec![InlineKeyboardButton::callback(BTN_BACK, "back_to_admin_panel")],
                            ]);

                            bot.edit_message_text(message.chat.id, message.id, "Select Top 10 period:").await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        "format_menu" => {
                            let keyboard = InlineKeyboardMarkup::new(vec![ 
                                vec![ 
                                    InlineKeyboardButton::callback("h265", "set_quality_h265"),
                                    InlineKeyboardButton::callback("h264", "set_quality_h264"),
                                    InlineKeyboardButton::callback("audio", "set_quality_audio"),
                                ],
                                vec![ 
                                    InlineKeyboardButton::callback(BTN_BACK, "back_to_settings"),
                                ]
                            ]);
                            let text = "h265: best quality, but may not work on some devices.\nh264: worse quality, but works on many devices.\naudio: audio only";
                            bot.edit_message_text(message.chat.id, message.id, text).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        "back_to_main" => {
                            let keyboard = InlineKeyboardMarkup::new(vec![vec![ 
                                InlineKeyboardButton::callback(BTN_SETTINGS, "settings"),
                            ]]);
                            bot.edit_message_text(message.chat.id, message.id, "Welcome! Send me a TikTok link.").await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        "back_to_settings" => {
                            let mut keyboard_rows = vec![vec![
                                InlineKeyboardButton::callback(BTN_FORMAT, "format_menu"),
                            ]];

                            if is_admin(&message).await {
                                keyboard_rows.push(vec![
                                    InlineKeyboardButton::callback(BTN_ADMIN_PANEL, "admin_panel"),
                                ]);
                            }

                            keyboard_rows.push(vec![
                                InlineKeyboardButton::callback(BTN_BACK, "back_to_main"),
                            ]);

                            let keyboard = InlineKeyboardMarkup::new(keyboard_rows);

                            bot.edit_message_text(message.chat.id, message.id, BTN_SETTINGS).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        "back_to_admin_panel" => {
                            if !is_admin(message).await {
                                bot.answer_callback_query(q.id).text("Access denied.").await?;
                                return Ok(());
                            }
                            let keyboard = InlineKeyboardMarkup::new(vec![
                                vec![InlineKeyboardButton::callback("Stats", "admin_stats")],
                                vec![InlineKeyboardButton::callback("Top 10", "admin_top10")],
                                vec![InlineKeyboardButton::callback("All users", "admin_users_page_0")],
                                vec![InlineKeyboardButton::callback(BTN_SUBSCRIPTION, "subscription_menu")],
                                vec![InlineKeyboardButton::callback(BTN_BACK, "back_to_settings")],
                            ]);
                            bot.edit_message_text(message.chat.id, message.id, BTN_ADMIN_PANEL).await?;
                            bot.edit_message_reply_markup(message.chat.id, message.id).reply_markup(keyboard).await?;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}
