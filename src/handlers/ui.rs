pub const BTN_ADMIN_PANEL: &str = "Admin Panel";
pub const BTN_ADMIN_MANAGEMENT: &str = "👑 Admin Management";
pub const BTN_ADD_ADMIN: &str = "➕ Add Admin";
pub const BTN_REMOVE_ADMIN: &str = "➖ Remove Admin";
pub const BTN_ADMIN_LIST: &str = "📋 Admin List";
pub const BTN_PREMIUM_GRANT_TOGGLE: &str = "Premium grants: ";
pub const BTN_PREMIUM_GRANT_FIRST_ONLY: &str = "First admin only";
pub const BTN_PREMIUM_GRANT_ALL: &str = "All admins";
pub const BTN_PREMIUM_MANAGEMENT: &str = "💎 Premium Management";
pub const BTN_ADD_PREMIUM: &str = "➕ Add Premium";
pub const BTN_REMOVE_PREMIUM: &str = "➖ Remove Premium";
pub const BTN_PREMIUM_LIST: &str = "📋 Premium List";
pub const BTN_ADMIN_PREMIUM_GRANTS: &str = "👑 Granted by Admins";
pub const BTN_PREMIUM_1H: &str = "⏱ 1 hour";
pub const BTN_PREMIUM_1D: &str = "📅 1 day";
pub const BTN_PREMIUM_7D: &str = "📅 7 days";
pub const BTN_PREMIUM_30D: &str = "📅 30 days";
pub const BTN_PREMIUM_90D: &str = "📅 90 days";
pub const BTN_PREMIUM_1Y: &str = "📅 1 year";
pub const BTN_PREMIUM_CUSTOM: &str = "✍️ Свой срок";
pub const BTN_CANCEL: &str = "❌ Cancel";
pub const BTN_SETTINGS: &str = "⚙️ Settings";
pub const BTN_FORMAT: &str = "Format";
pub const BTN_SUBSCRIPTION: &str = "Subscription";
pub const BTN_SUBSCRIPTION_TOGGLE: &str = "Subscription: ";
pub const BTN_SUBSCRIPTION_ADMIN_TOGGLE: &str = "Subscription admin: ";
pub const BTN_SET_CHANNELS: &str = "Set channels";
pub const BTN_TOGGLE_ADS: &str = "Ads: ";
pub const BTN_TOGGLE_SUCCESS_NOTIFS: &str = "Notify Success: ";
pub const BTN_TOGGLE_FAIL_NOTIFS: &str = "Notify Fail: ";
pub const BTN_BACK: &str = "Back";
pub const BTN_LANGUAGE: &str = "🌐 Language";
pub const BTN_AUTO_DETECT: &str = "🌐 Auto-detect";
pub const BTN_PRICE: &str = "💰 Price";

pub fn is_menu_button(text: &str) -> bool {
    matches!(
        text,
        BTN_ADMIN_PANEL
            | BTN_ADMIN_MANAGEMENT
            | BTN_PREMIUM_MANAGEMENT
            | BTN_SETTINGS
            | BTN_FORMAT
            | BTN_SUBSCRIPTION
            | BTN_BACK
            | BTN_LANGUAGE
    )
}

pub fn is_system_button(text: &str) -> bool {
    matches!(
        text,
        BTN_ADMIN_PANEL
            | BTN_ADMIN_MANAGEMENT
            | BTN_ADD_ADMIN
            | BTN_REMOVE_ADMIN
            | BTN_ADMIN_LIST
            | BTN_PREMIUM_MANAGEMENT
            | BTN_ADD_PREMIUM
            | BTN_REMOVE_PREMIUM
            | BTN_PREMIUM_LIST
            | BTN_ADMIN_PREMIUM_GRANTS
            | BTN_PREMIUM_1H
            | BTN_PREMIUM_1D
            | BTN_PREMIUM_7D
            | BTN_PREMIUM_30D
            | BTN_PREMIUM_90D
            | BTN_PREMIUM_1Y
            | BTN_PREMIUM_CUSTOM
            | BTN_CANCEL
            | BTN_SETTINGS
            | BTN_FORMAT
            | BTN_SUBSCRIPTION
            | BTN_BACK
            | BTN_LANGUAGE
            | BTN_AUTO_DETECT
            | BTN_SET_CHANNELS
            | "📢 Broadcast"
            | "📊 Stats"
            | "🏆 Top 10"
            | "👥 All users"
            | "💎 Premium Users"
            | "➕ Add Premium User"
            | "📈 Stats"
            | "📅 Week"
            | "🔻 Funnel"
            | "h265"
            | "h264"
            | "audio"
            | BTN_PRICE
    ) || text.starts_with(BTN_PREMIUM_GRANT_TOGGLE)
        || text.starts_with(BTN_SUBSCRIPTION_TOGGLE)
        || text.starts_with(BTN_SUBSCRIPTION_ADMIN_TOGGLE)
        || text.starts_with(BTN_TOGGLE_ADS)
        || text.starts_with(BTN_TOGGLE_SUCCESS_NOTIFS)
        || text.starts_with(BTN_TOGGLE_FAIL_NOTIFS)
        || text.starts_with(BTN_PRICE)
        || crate::i18n::is_language_button(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_menu_button() {
        assert!(is_menu_button(BTN_ADMIN_PANEL));
        assert!(is_menu_button(BTN_SETTINGS));
        assert!(is_menu_button(BTN_FORMAT));
        assert!(is_menu_button(BTN_SUBSCRIPTION));
        assert!(is_menu_button(BTN_BACK));
        assert!(is_menu_button(BTN_LANGUAGE));
        assert!(!is_menu_button(BTN_AUTO_DETECT));
        assert!(!is_menu_button("some other text"));
    }

    #[test]
    fn auto_detect_is_system_but_not_menu() {
        assert!(is_system_button(BTN_AUTO_DETECT));
        assert!(is_system_button(BTN_LANGUAGE));
        assert!(is_system_button(BTN_PRICE));
        assert!(is_system_button(&format!("{}: 50 ⭐", BTN_PRICE)));
    }

    #[test]
    fn subscription_submenu_buttons_are_system_but_not_menu() {
        assert!(is_system_button(BTN_SET_CHANNELS));
        assert!(is_system_button(&format!(
            "{}ON ✅",
            BTN_SUBSCRIPTION_TOGGLE
        )));
        assert!(is_system_button(&format!(
            "{}OFF ❌",
            BTN_SUBSCRIPTION_ADMIN_TOGGLE
        )));
        assert!(!is_menu_button(BTN_SET_CHANNELS));
        assert!(!is_menu_button(&format!(
            "{}ON ✅",
            BTN_SUBSCRIPTION_TOGGLE
        )));
    }
}
