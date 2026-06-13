use teloxide::prelude::*;
use teloxide::types::{LabeledPrice, PreCheckoutQuery, Message};
use std::sync::Arc;
use crate::database::DatabasePool;
use std::env;

pub const PREMIUM_PAYLOAD: &str = "premium_30_days_xtr";
pub const CURRENCY_XTR: &str = "XTR";

/// Validate if the payload matches the expected premium purchase payload
pub fn is_valid_premium_payload(payload: &str) -> bool {
    payload == PREMIUM_PAYLOAD
}

/// Validate if the currency is Telegram Stars (XTR)
pub fn is_xtr_currency(currency: &str) -> bool {
    currency == CURRENCY_XTR
}

/// 1. Send Invoice (Telegram Stars)
pub async fn send_premium_invoice(bot: Bot, chat_id: ChatId, db_pool: Arc<DatabasePool>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let price_val: u32 = env::var("PREMIUM_STARS_PRICE")
        .unwrap_or_else(|_| "50".to_string())
        .parse()
        .unwrap_or(50);
    
    log::info!("[PAYMENT_CHAIN] 1. Initiation: User={}, Amount={} Stars", chat_id, price_val);

    // Log the invoice being sent
    let _ = db_pool.log_invoice(chat_id.0, price_val as i64, PREMIUM_PAYLOAD).await;

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

/// Logic for approving/rejecting PreCheckoutQuery
pub fn validate_pre_checkout(payload: &str, currency: &str) -> bool {
    is_valid_premium_payload(payload) && is_xtr_currency(currency)
}

/// 2. Handle PreCheckoutQuery (Crucial step to stop the loading spinner)
pub async fn handle_pre_checkout(bot: Bot, q: PreCheckoutQuery) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let query_id = q.id.clone();
    log::info!(
        "[PAYMENT_CHAIN] 3. PreCheckoutQuery received! ID={}, User={}, Payload={}, Amount={}",
        query_id, q.from.id, q.invoice_payload, q.total_amount
    );

    let ok = validate_pre_checkout(&q.invoice_payload, &q.currency);

    if !ok {
        log::warn!("[PAYMENT_CHAIN] ❌ Rejecting PreCheckout {}: invalid payload or currency", query_id);
        bot.answer_pre_checkout_query(query_id, false)
            .error_message("Error: invalid order identifier or currency.")
            .await?;
        return Ok(());
    }

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

/// Core logic for processing a successful payment without Telegram API dependencies
pub async fn process_successful_payment_logic(
    user_id: i64,
    payload: &str,
    currency: &str,
    amount: i32,
    db_pool: &DatabasePool,
) -> Result<bool, anyhow::Error> {
    if !is_valid_premium_payload(payload) || !is_xtr_currency(currency) {
        return Ok(false);
    }

    log::info!("[PAYMENT_CHAIN] 6. Granting premium in DB for user {}", user_id);
    db_pool.set_user_premium(user_id, 30).await?;
    
    // Log the successful payment
    let _ = db_pool.log_payment(user_id, amount as i64, payload).await;
    
    Ok(true)
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

    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

    log::info!(
        "[PAYMENT_CHAIN] 5. FINAL: SuccessfulPayment received! User={}, Payload={}, Total={} {}",
        user_id,
        payment.invoice_payload,
        payment.total_amount,
        payment.currency
    );

    let success = process_successful_payment_logic(
        user_id,
        &payment.invoice_payload,
        &payment.currency,
        payment.total_amount as i32,
        &db_pool
    ).await?;

    if success {
        if let Some(user) = &msg.from {
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
    } else {
        log::error!("[PAYMENT_CHAIN] ❌ Validation failed for successful payment from user {}", user_id);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_premium_payload() {
        assert!(is_valid_premium_payload(PREMIUM_PAYLOAD));
        assert!(!is_valid_premium_payload("premium_60_days"));
        assert!(!is_valid_premium_payload(""));
    }

    #[test]
    fn test_xtr_currency() {
        assert!(is_xtr_currency(CURRENCY_XTR));
        assert!(!is_xtr_currency("USD"));
        assert!(!is_xtr_currency("RUB"));
        assert!(!is_xtr_currency(""));
    }
}
