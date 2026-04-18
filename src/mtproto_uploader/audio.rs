use crate::peers::resolve_peer;

use anyhow;
use grammers_tl_types as tl;

use std::path::Path;
use log;

use crate::utils::progress_bar::ProgressBar;

use crate::mtproto_uploader::uploader::MTProtoUploader; // Import MTProtoUploader
use crate::mtproto_uploader::file_uploader::upload_file_in_parts_with_reconnect;

impl MTProtoUploader {
    pub async fn upload_audio(
        &self,
        chat_id: i64,
        username: Option<String>,
        file_path: &Path,
        caption: &str,
        progress_bar: &mut ProgressBar,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Upload the audio file using reconnect mechanism
        let (file_id, total_parts) = upload_file_in_parts_with_reconnect(self, file_path, progress_bar, "audio").await.map_err(|e| {
            log::error!("Failed to upload audio file {:?}: {:?}", file_path, e);
            e
        })?;

        // Access the actual client through the mutex
        let client = self.client.lock().await;
        
        let input_peer = resolve_peer(&self.client, chat_id, username.as_deref()).await.map_err(|e| {
            log::error!("Failed to resolve peer: {:?}", e);
            e
        })?;

        let input_file = tl::enums::InputFile::Big(tl::types::InputFileBig {
            id: file_id,
            parts: total_parts,
            name: file_path
                .file_name()
                .and_then(|os_str| os_str.to_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    log::error!("Failed to extract file name from path: {:?}", file_path);
                    anyhow::anyhow!("Failed to extract file name from path")
                })?,
        });

        let ext = file_path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        let mime = match ext.as_str() {
            "mp3" => "audio/mpeg",
            "m4a" => "audio/mp4",
            "aac" => "audio/aac",
            "ogg" => "audio/ogg",
            _ => "audio/mpeg",
        }.to_string();

        let audio_attr = tl::enums::DocumentAttribute::Audio(tl::types::DocumentAttributeAudio {
            voice: false,
            duration: 0,              // optionally calculate beforehand
            title: None,
            performer: None,
            waveform: None,
        });

        let media = tl::enums::InputMedia::UploadedDocument(tl::types::InputMediaUploadedDocument {
            nosound_video: false,
            spoiler: false,
            file: input_file,
            thumb: None,
            mime_type: mime,
            force_file: false,
            attributes: vec![audio_attr],
            stickers: Some(Vec::new()),
            ttl_seconds: None,
            video_cover: None,
            video_timestamp: None,
        });

        let random_id = rand::random();
        
        // Sending message
        let request = tl::functions::messages::SendMedia {
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            allow_paid_floodskip: false,
            allow_paid_stars: None,
            schedule_repeat_period: None,
            suggested_post: None,
            peer: input_peer,
            reply_to: None,
            media,
            message: caption.to_string(),
            random_id,
            reply_markup: None,
            entities: Some(Vec::new()),
            schedule_date: None,
            send_as: None,
            effect: None,
            invert_media: false,
            quick_reply_shortcut: None,
        };
        
        client.invoke(&request).await.map_err(|e| {
            log::error!("Failed to send audio: {:?}", e);
            e
        })?;
        
        Ok(())
    }
}