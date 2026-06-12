# Telegram TikTok Downloader Bot (Rust)

A powerful, high-performance Telegram bot written in Rust for downloading TikTok videos (no watermarks), Instagram reels, and YouTube Shorts. Optimized for scalability, security, and monetization.

## 🌟 Key Features

-   **Multi-Platform Support**: TikTok, Instagram, YouTube, and more (via `yt-dlp`).
-   **Monetization (Monetag)**: Integrated Rewarded Interstitial ads with **Server-to-Server (S2S) verification**.
-   **Premium Subscriptions**: Support for **Telegram Stars** (XTR) to bypass ads and unlock instant downloads.
-   **Advanced Admin Panel**:
    -   Real-time stats (Total Users, Downloads).
    -   Global Broadcast system.
    -   Manual Premium User management.
    -   Granular notification toggles (Success/Fail alerts).
    -   Ad system master switch.
-   **Auto-Update System**: Automatically monitors and downloads the latest `yt-dlp` and `FFmpeg` binaries.
-   **MTProto Support**: High-speed uploads for large files (up to 2GB) using the Telegram MTProto protocol.
-   **Global Test Mode**: Seamless switching between Telegram Production and Test servers.

## 🛠 Tech Stack

-   **Language**: Rust (Edition 2024)
-   **Framework**: [Teloxide](https://github.com/teloxide/teloxide) (Telegram Bot Framework)
-   **Web Server**: [Axum](https://github.com/tokio-rs/axum) (for Mini App and Webhooks)
-   **Database**: SQLite (via `rusqlite` and `r2d2` pool)
-   **MTProto**: `grammers` (for large file handling)

## 💰 Monetization & "Double Lock" Verification

The bot uses a unique "Double Lock" system to ensure high ad completion rates:
1.  **S2S Postback**: The bot's API receives a verified signal from Monetag when an ad is completed.
2.  **Mini App Claim**: Users must click "Claim" in the Telegram Mini App to trigger the download after verification.
3.  **Admin Bypass**: Admins can bypass verification during testing to ensure smooth flows.

## 🚀 Installation & Setup

### Prerequisites
-   Rust 1.83.0+
-   A Telegram Bot Token (from [@BotFather](https://t.me/BotFather))
-   Monetag account (for Ads)

### 1. Configuration
Create a `.env` file in the root directory:

```env
TELOXIDE_TOKEN=your_bot_token
ADMIN_IDS=12345678,87654321
CHANNEL_IDS=-100... (mandatory sub channels)

# MTProto Credentials (my.telegram.org)
TELEGRAM_API_ID=12345
TELEGRAM_API_HASH=your_hash

# Mini App & Ads
WEBAPP_URL=https://your-domain.com
MONETAG_ZONE_ID=11093538
MONETAG_MODULE_ENABLED=true
WEB_SERVER_PORT=8088

# Global Toggles
TEST_MODE=false
SUBSCRIPTION_REQUIRED=true
```

### 2. Run
```bash
cargo run --release
```
*Note: Binaries (`yt-dlp`, `ffmpeg`) will be automatically downloaded on the first run.*

## 🧪 Telegram Test Server Support

To test features like Telegram Stars without spending real money:
1.  Set `TEST_MODE=true` in `.env`.
2.  Use a test token from BotFather in the Test environment.
3.  Delete `telegram.session` before restarting.
4.  The bot will automatically connect to **Amsterdam DC2**.

## 📊 Admin Commands

-   `/start` - Access main menu.
-   `Admin Panel` button - Access the granular control interface.
-   `➕ Add Premium User` - Grant 30 days of Premium to a specific ID.
-   `Ads: ON/OFF` - Instant global ad toggle.

## 📝 License

This project is licensed under the MIT License.
