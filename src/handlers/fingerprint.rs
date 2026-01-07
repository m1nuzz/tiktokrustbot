use crate::database::DatabasePool;
use crate::handlers::admin::is_admin;
use std::process::Command;
use std::sync::Arc;
use teloxide::prelude::*;

/// Парсить вивід yt-dlp --list-impersonate-targets
fn parse_impersonate_targets(output: &str) -> Vec<(String, String)> {
    let mut targets = Vec::new();
    let mut lines = output.lines().skip(3);

    while let Some(line) = lines.next() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let client = parts[0];
            let os = parts[1];
            let source = parts[2];
            if !source.contains("unavailable") {
                let target = if os == "-" {
                    client.to_string()
                } else {
                    format!("{}:{}", client, os)
                };

                // 🔥 ДОДАЛИ .to_lowercase() - це ключова зміна!
                targets.push((target.to_lowercase(), format!("{} {}", target, source)));
            }
        }
    }
    targets
}

pub async fn fingerprint_list_handler(
    bot: Bot,
    msg: Message,
    ytdlp_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "❌ This command is for admins only.")
            .await?;
        return Ok(());
    }

    let output = Command::new(ytdlp_path)
        .arg("--list-impersonate-targets")
        .output();

    match output {
        Ok(result) => {
            let stdout = String::from_utf8_lossy(&result.stdout);

            if !result.status.success() {
                bot.send_message(
                    msg.chat.id,
                    "❌ Could not get fingerprint list. Make sure `curl-cffi` is installed:\n\n\
                    `pip install yt-dlp[curl-cffi]`",
                )
                .await?;
                return Ok(());
            }

            let targets = parse_impersonate_targets(&stdout);

            if targets.is_empty() {
                bot.send_message(
                    msg.chat.id,
                    "❌ No available fingerprints found. Make sure `curl-cffi` is installed:\n\n\
                    `pip install yt-dlp[curl-cffi]`",
                )
                .await?;
                return Ok(());
            }

            let mut response = String::from("📱 <b>Available TLS Fingerprints:</b>\n\n");
            for (target, _description) in targets {
                response.push_str(&format!(
                    "• <code>{}</code> - <code>/setfingerprint-{}</code>\n",
                    target, target
                ));
            }

            response.push_str("\n🔓 <b>Disable fingerprint:</b>\n");
            response.push_str("• <code>disable</code> - <code>/setfingerprint-disable</code>\n");

            response.push_str("\n💡 Click on the command to set the fingerprint.");

            bot.send_message(msg.chat.id, response)
                .parse_mode(teloxide::types::ParseMode::Html)
                .await?;
        }
        Err(e) => {
            log::error!("Failed to execute yt-dlp: {}", e);
            bot.send_message(msg.chat.id, "❌ Error executing yt-dlp.")
                .await?;
        }
    }

    Ok(())
}

pub async fn set_fingerprint_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
    fingerprint: String,
    ytdlp_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "❌ This command is for admins only.")
            .await?;
        return Ok(());
    }

    let fingerprint_lower = fingerprint.to_lowercase();

    if fingerprint_lower == "disable" {
        let result = db_pool
            .execute_with_timeout(move |conn| {
                conn.execute("DELETE FROM settings WHERE key = 'tls_fingerprint'", [])
            })
            .await;

        match result {
            Ok(_) => {
                log::info!("TLS fingerprint is disabled.");
                bot.send_message(msg.chat.id, "✅ TLS fingerprint has been disabled.")
                    .await?;
            }
            Err(e) => {
                log::error!("Failed to disable fingerprint: {}", e);
                bot.send_message(
                    msg.chat.id,
                    "❌ An error occurred while disabling the fingerprint.",
                )
                .await?;
            }
        }
        return Ok(());
    }

    let output = Command::new(ytdlp_path)
        .arg("--list-impersonate-targets")
        .output();

    match output {
        Ok(result) => {
            if !result.status.success() {
                bot.send_message(
                    msg.chat.id,
                    "❌ Could not get fingerprint list. Make sure `curl-cffi` is installed:\n\n\
                    `pip install yt-dlp[curl-cffi]`",
                )
                .await?;
                return Ok(());
            }

            let stdout = String::from_utf8_lossy(&result.stdout);
            let targets = parse_impersonate_targets(&stdout);

            let is_valid = targets
                .iter()
                .any(|(target, _)| target == &fingerprint_lower);

            if !is_valid {
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "❌ Fingerprint <code>{}</code> not found. Use /fingerprint to see the list.",
                        fingerprint
                    ),
                )
                .parse_mode(teloxide::types::ParseMode::Html)
                .await?;
                return Ok(());
            }

            let fp_clone = fingerprint_lower.clone();
            let result = db_pool
                .execute_with_timeout(move |conn| {
                    conn.execute(
                        "INSERT OR REPLACE INTO settings (key, value) VALUES ('tls_fingerprint', ?1)",
                        rusqlite::params![fp_clone],
                    )
                })
                .await;

            match result {
                Ok(_) => {
                    log::info!("TLS fingerprint changed to: {}", fingerprint_lower);
                    bot.send_message(
                        msg.chat.id,
                        format!(
                            "✅ TLS fingerprint set to: <code>{}</code>.",
                            fingerprint_lower
                        ),
                    )
                    .parse_mode(teloxide::types::ParseMode::Html)
                    .await?;
                }
                Err(e) => {
                    log::error!("Failed to save fingerprint: {}", e);
                    bot.send_message(msg.chat.id, "❌ Error saving fingerprint.")
                        .await?;
                }
            }
        }
        Err(e) => {
            log::error!("Failed to execute yt-dlp: {}", e);
            bot.send_message(msg.chat.id, "❌ Error executing yt-dlp.")
                .await?;
        }
    }

    Ok(())
}

pub async fn get_current_fingerprint(dbpool: Arc<DatabasePool>) -> Option<String> {
    let result = dbpool
        .execute_with_timeout(|conn| {
            conn.query_row(
                "SELECT value FROM settings WHERE key = 'tls_fingerprint'",
                [],
                |row| row.get::<_, String>(0),
            )
        })
        .await;

    result.ok()
}
