pub const BTN_ADMIN_PANEL: &str = "Admin Panel";
pub const BTN_SETTINGS: &str = "⚙️ Settings";
pub const BTN_FORMAT: &str = "Format";
pub const BTN_SUBSCRIPTION: &str = "Subscription";
pub const BTN_TOGGLE_ADS: &str = "Ads: ";
pub const BTN_TOGGLE_SUCCESS_NOTIFS: &str = "Notify Success: ";
pub const BTN_TOGGLE_FAIL_NOTIFS: &str = "Notify Fail: ";
pub const BTN_BACK: &str = "Back";
pub const BTN_LANGUAGE: &str = "🌐 Language";
pub const BTN_AUTO_DETECT: &str = "🌐 Auto-detect";
pub const BTN_PRICE: &str = "💰 Price";

pub fn is_menu_button(text: &str) -> bool {
    matches!(text,
        BTN_ADMIN_PANEL |
        BTN_SETTINGS |
        BTN_FORMAT |
        BTN_SUBSCRIPTION |
        BTN_BACK |
        BTN_LANGUAGE
    )
}

pub fn is_system_button(text: &str) -> bool {
    matches!(
        text,
        BTN_ADMIN_PANEL | BTN_SETTINGS | BTN_FORMAT | BTN_SUBSCRIPTION | BTN_BACK |
        BTN_LANGUAGE | BTN_AUTO_DETECT |
        "📢 Broadcast" | "📊 Stats" | "🏆 Top 10" | "👥 All users" | "💎 Premium Users" | "➕ Add Premium User" |
        "📈 Daily Stats" | "📅 Week" | "🔻 Funnel" |
        "h265" | "h264" | "audio" | BTN_PRICE
    ) || text.starts_with(BTN_TOGGLE_ADS)
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
}