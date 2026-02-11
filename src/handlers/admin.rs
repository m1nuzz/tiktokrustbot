use std::env;
use teloxide::prelude::*;

pub async fn is_admin(msg: &Message) -> bool {
    let admin_ids_str = env::var("ADMIN_IDS").unwrap_or_default();
    let admin_ids: Vec<i64> = admin_ids_str
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    // Check user ID instead of chat ID
    if let Some(user) = msg.from.as_ref() {
        admin_ids.contains(&(user.id.0 as i64))
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    // Helper function to test parsing logic without environment variables
    fn parse_admin_ids(admin_ids_str: &str) -> Vec<i64> {
        admin_ids_str
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect()
    }

    #[test]
    fn test_parse_admin_ids() {
        // Test that admin IDs are parsed correctly
        let admin_ids = parse_admin_ids("123456,789012, 345678");
        assert_eq!(admin_ids, vec![123456, 789012, 345678]);
    }

    #[test]
    fn test_parse_admin_ids_empty() {
        // Test that empty admin IDs list works correctly
        let admin_ids = parse_admin_ids("");
        assert_eq!(admin_ids, Vec::<i64>::new());
    }

    #[test]
    fn test_parse_admin_ids_with_spaces() {
        // Test that admin IDs with spaces are parsed correctly
        let admin_ids = parse_admin_ids(" 111111 , 222222 , 333333 ");
        assert_eq!(admin_ids, vec![111111, 222222, 333333]);
    }

    #[test]
    fn test_admin_id_matching() {
        // Test the core logic: checking if a user ID is in the admin list
        let admin_ids = vec![123456i64, 789012i64];
        
        // Admin user ID should match
        let admin_user_id: i64 = 123456;
        assert!(admin_ids.contains(&admin_user_id));
        
        // Non-admin user ID should not match
        let regular_user_id: i64 = 555555;
        assert!(!admin_ids.contains(&regular_user_id));
    }

    #[test]
    fn test_user_id_type_conversion() {
        // Test that u64 user IDs can be compared with i64 admin IDs
        let admin_ids = vec![123456i64, 789012i64];
        
        // Simulate a user ID from Telegram (which is u64)
        let telegram_user_id: u64 = 123456;
        
        // This is what happens in the is_admin function
        let is_admin = admin_ids.contains(&(telegram_user_id as i64));
        assert!(is_admin);
        
        // Non-admin should not match
        let regular_user_id: u64 = 555555;
        let is_admin = admin_ids.contains(&(regular_user_id as i64));
        assert!(!is_admin);
    }
}
