use teloxide::types::{Update, UpdateKind, Message};
use serde_json::json;

fn main() {
    let PREMIUM_PAYLOAD = "premium_30_days_xtr";
    let CURRENCY_XTR = "XTR";

    let update_json = json!({
        "update_id": 1,
        "message": {
            "message_id": 1,
            "date": 1622548800,
            "chat": { "id": 123, "type": "private", "first_name": "Test" },
            "from": { "id": 123, "is_bot": false, "first_name": "Test" },
            "successful_payment": {
                "currency": CURRENCY_XTR,
                "total_amount": 50,
                "invoice_payload": PREMIUM_PAYLOAD,
                "telegram_payment_charge_id": "t_id",
                "provider_payment_charge_id": "p_id",
                "is_recurring": false,
                "is_first_recurring": false
            }
        }
    });

    let update: Result<Update, _> = serde_json::from_value(update_json);
    match update {
        Ok(u) => println!("Update: {:?}", u),
        Err(e) => println!("Update Error: {:?}", e),
    }

    // Try to see what UpdateKind variants are
    // We can't easily list them at runtime without reflection, but we can see the Debug output
}
