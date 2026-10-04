use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, InputMedia, InputMediaPhoto};

use crate::utils::progress_bar::ProgressBar;
use crate::yt_dlp_interface::{PhotoPostFiles, TikwmMeta, YoutubeFetcher};

/// Split a photo count into sendMediaGroup chunks. Telegram allows 2-10 items
/// per group; a lone photo goes via send_photo instead.
pub fn split_media_chunks(total: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut rest = total;
    while rest > 10 {
        out.push(10);
        rest -= 10;
    }
    if rest > 0 {
        out.push(rest);
    }
    out
}

/// Send downloaded carousel images as albums (single photos via send_photo).
pub async fn send_photo_post(
    bot: &Bot,
    chat_id: ChatId,
    files: &PhotoPostFiles,
    progress_bar: &mut ProgressBar,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut offset = 0;
    for size in split_media_chunks(files.images.len()) {
        let chunk = &files.images[offset..offset + size];
        offset += size;
        if size == 1 {
            bot.send_photo(chat_id, InputFile::file(&chunk[0])).await?;
        } else {
            let media: Vec<InputMedia> = chunk
                .iter()
                .map(|p| InputMedia::Photo(InputMediaPhoto::new(InputFile::file(p))))
                .collect();
            bot.send_media_group(chat_id, media).await?;
        }
        progress_bar.update(90, Some("📤 Uploading photos...")).await?;
    }
    Ok(())
}

/// Full photo-post pipeline: download images + soundtrack, send the album(s),
/// then the audio track (unless the user asked for audio only, in which case
/// only the track is sent).
pub async fn handle_photo_post(
    bot: &Bot,
    chat_id: ChatId,
    fetcher: &YoutubeFetcher,
    meta: &TikwmMeta,
    filename_stem: &str,
    audio_only: bool,
    progress_bar: &mut ProgressBar,
) -> anyhow::Result<()> {
    let files: PhotoPostFiles = fetcher
        .download_photo_post(meta, filename_stem, audio_only, progress_bar)
        .await?;
    if !audio_only {
        send_photo_post(bot, chat_id, &files, progress_bar)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send photos: {}", e))?;
    }
    if let Some(audio_path) = &files.audio {
        progress_bar.update(95, Some("📤 Uploading audio...")).await?;
        crate::telegram_bot_api_uploader::send_audio_with_progress_botapi(
            &bot.token(),
            chat_id,
            audio_path,
            None,
            progress_bar,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_chunks_cover_telegram_limits() {
        assert_eq!(split_media_chunks(0), Vec::<usize>::new());
        assert_eq!(split_media_chunks(1), vec![1]);
        assert_eq!(split_media_chunks(10), vec![10]);
        assert_eq!(split_media_chunks(11), vec![10, 1]);
        assert_eq!(split_media_chunks(23), vec![10, 10, 3]);
        assert_eq!(split_media_chunks(35), vec![10, 10, 10, 5]);
    }
}
