pub const BTN_ADMIN_PANEL: &str = "Admin Panel";
pub const BTN_SETTINGS: &str = "⚙️ Settings";
pub const BTN_FORMAT: &str = "Format";
pub const BTN_SUBSCRIPTION: &str = "Subscription";
pub const BTN_BACK: &str = "Back";

pub fn is_menu_button(text: &str) -> bool {
    matches!(text,
        BTN_ADMIN_PANEL |
        BTN_SETTINGS |
        BTN_FORMAT |
        BTN_SUBSCRIPTION |
        BTN_BACK
    )
}

pub fn is_system_button(text: &str) -> bool {
    matches!(
        text,
        BTN_ADMIN_PANEL | BTN_SETTINGS | BTN_FORMAT | BTN_SUBSCRIPTION | BTN_BACK |
        "📢 Broadcast" | "📊 Stats" | "🏆 Top 10" | "👥 All users" |
        "h265" | "h264" | "audio"
    )
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
        assert!(!is_menu_button("some other text"));
    }
}