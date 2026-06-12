use teloxide::prelude::*;
use teloxide::types::{LabeledPrice, PreCheckoutQuery, Message};
use std::sync::Arc;
use crate::database::DatabasePool;
use std::env;

const PREMIUM_PAYLOAD: &str = "premium_30_days_xtr";
const CURRENCY_XTR: &str = "XTR";

/// 1. Send Invoice (Telegram Stars)
pub async fn send_premium_invoice(bot: Bot, chat_id: ChatId) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let price_val: u32 = env::var("PREMIUM_STARS_PRICE")
        .unwrap_or_else(|_| "50".to_string())
        .parse()
        .unwrap_or(50);
    
    log::info!("[PAYMENT_CHAIN] 1. Initiation: User={}, Amount={} Stars", chat_id, price_val);

    match bot.send_invoice(
        chat_id,
        "Premium", // Header
        "✨ Remove ad (Buy Premium) for 1 month!", // Ordinary text (description)
        PREMIUM_PAYLOAD,
        CURRENCY_XTR,
        vec![LabeledPrice::new("Premium Status", price_val)],
    )
    .await {
        Ok(_) => {
            log::info!("[PAYMENT_CHAIN] 2. Invoice sent to user {}", chat_id);
        }
        Err(e) => {
            log::error!("[PAYMENT_CHAIN] ❌ Error sending invoice to {}: {:?}", chat_id, e);
        }
    }

    Ok(())
}

/// 2. Handle PreCheckoutQuery (Crucial step to stop the loading spinner)
pub async fn handle_pre_checkout(bot: Bot, q: PreCheckoutQuery) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let query_id = q.id.clone();
    log::info!(
        "[PAYMENT_CHAIN] 3. PreCheckoutQuery received! ID={}, User={}, Payload={}, Amount={}",
        query_id, q.from.id, q.invoice_payload, q.total_amount
    );

    // Payload validation
    if q.invoice_payload != PREMIUM_PAYLOAD {
        log::warn!("[PAYMENT_CHAIN] ❌ Rejecting PreCheckout {}: invalid payload '{}'", query_id, q.invoice_payload);
        bot.answer_pre_checkout_query(query_id, false)
            .error_message("Error: invalid order identifier.")
            .await?;
        return Ok(());
    }

    // Answer OK immediately
    match bot.answer_pre_checkout_query(query_id.clone(), true).await {
        Ok(_) => {
            log::info!("[PAYMENT_CHAIN] 4. PreCheckout approved (OK) for ID={}", query_id);
        }
        Err(e) => {
            log::error!("[PAYMENT_CHAIN] ❌ Error answering PreCheckout {}: {:?}", query_id, e);
        }
    }
    
    Ok(())
}

/// 3. Handle Successful Payment
pub async fn handle_successful_payment(
    bot: Bot, 
    msg: Message, 
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let payment = match msg.successful_payment() {
        Some(p) => p,
        None => return Ok(()),
    };

    log::info!(
        "[PAYMENT_CHAIN] 5. FINAL: SuccessfulPayment received! User={}, Payload={}, Total={} {}",
        msg.chat.id,
        payment.invoice_payload,
        payment.total_amount,
        payment.currency
    );

    // Final validation
    if payment.invoice_payload != PREMIUM_PAYLOAD {
        log::error!("[PAYMENT_CHAIN] ❌ CRITICAL: Payload mismatch! Expected {}, got {}", PREMIUM_PAYLOAD, payment.invoice_payload);
        return Ok(());
    }

    if let Some(user) = &msg.from {
        log::info!("[PAYMENT_CHAIN] 6. Granting premium in DB for user {}", user.id);
        match db_pool.set_user_premium(user.id.0 as i64, 30).await {
            Ok(_) => {
                bot.send_message(msg.chat.id, "🎉 Success! Premium activated for 30 days! ✨").await?;
                log::info!("[PAYMENT_CHAIN] 7. Payment completed. User {} (ID: {}) is now Premium.", user.first_name, user.id);
                
                let notify_success = db_pool.get_setting("notify_success").await.map(|v| v == "true").unwrap_or(true);
                
                if notify_success {
                    let admin_ids_str = env::var("ADMIN_IDS").unwrap_or_default();
                    let admin_ids: Vec<i64> = admin_ids_str
                        .split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    
                    let notify_text = format!(
                        "💰 [STARS] Purchase! User @{} (ID: {}) bought Premium for {} Stars.",
                        user.username.as_deref().unwrap_or("unknown"),
                        user.id,
                        payment.total_amount
                    );
                    
                    for admin_id in admin_ids {
                        let _ = bot.send_message(ChatId(admin_id), &notify_text).await;
                    }
                }
            }
            Err(e) => {
                log::error!("[PAYMENT_CHAIN] ❌ DB Error granting premium to {}: {}", user.id, e);
                bot.send_message(msg.chat.id, "❌ Database error during activation. Please contact support.").await?;
            }
        }
    }

    Ok(())
}
