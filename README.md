# Telegram TikTok Downloader Bot

A Telegram bot written in Rust for downloading TikTok videos without watermarks, with support for various social media platforms including TikTok, Instagram, YouTube Shorts, and more.

## Features

- **Multi-platform support**: Download videos from TikTok, Instagram, YouTube Shorts, and other social media platforms
- **No watermarks**: Download clean videos without watermarks
- **High-quality downloads**: Automatically selects the best available quality
- **Telegram interface**: Easy-to-use bot interface within Telegram
- **Automatic updates**: Built-in auto-update functionality for yt-dlp and FFmpeg binaries
- **Database support**: Stores user information and download history
- **Admin commands**: Administrative features for channel management and Ad control
- **Monetization**: Integrated Telegram Mini App with Monetag ads (Rewarded Interstitial)
- **Safe Verification**: Server-to-Server (S2S) postback verification for ad rewards
- **Cross-platform**: Runs on Windows, Linux, and macOS

## Telegram Mini App & Ads

The bot includes a built-in Axum web server to handle a Telegram Mini App for monetization. Users are required to watch a Rewarded Interstitial ad before their video is processed.

### How it works:
1. User sends a link.
2. Bot generates a unique `ymid` and sends an invitation to the Mini App.
3. User watches the ad in the Mini App.
4. Monetag sends a secure S2S Postback to the bot's API.
5. User clicks "Get Video" in the Mini App (additional Popup monetization).
6. Bot releases the video to the user's chat.

## Configuration

The bot can be configured using environment variables in the `.env` file:
- `TELOXIDE_TOKEN`: Your Telegram bot token
- `ADMIN_IDS`: Comma-separated list of admin IDs
- `DATABASE_PATH`: Path to the SQLite database file
- `WEB_SERVER_PORT`: Port for the Axum web server (default: 8088)
- `WEBAPP_URL`: Your production domain (e.g., `https://cdnapi52.mooo.com`)
- `MONETAG_ZONE_ID`: Your Monetag Zone ID
- `MONETAG_MODULE_ENABLED`: Set to `true` or `false` to toggle the entire ad system

## Deployment (Nginx)

To run the bot in production with HTTPS, use Nginx as a reverse proxy:

```nginx
server {
    listen 8443 ssl; # Or 443
    server_name cdnapi52.mooo.com;

    location / {
        proxy_pass http://127.0.0.1:8088;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

## Monetag S2S Setup

In your Monetag dashboard, set the **Postback URL** for your Rewarded Interstitial zone to:
`https://your-domain.com/api/monetag-postback?ymid={ymid}&status={reward_event_type}`

## Contributing

Contributions are welcome! Please feel free to fork the repository and submit pull requests.

## License

This project is licensed under the MIT License.
