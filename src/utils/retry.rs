
use regex::Regex;

/// Extract FLOOD_WAIT seconds from Telegram error
pub fn extract_flood_wait(error_str: &str) -> Option<u64> {
    let re = Regex::new(r"FLOOD_WAIT_(\d+)").unwrap();
    re.captures(error_str)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse().ok())
}