//! Centralised user-facing localisation.
//!
//! All bot replies go through [`t()`]. Locales resolve from the Telegram
//! `User.language_code` IETF tag: exact match (`pt-br`) wins, then the base
//! subtag (`pt`), then English. Unknown tags fall back to English, so a
//! missing translation can never break a reply.

/// Message identifiers for every localised bot string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MsgKey {
    Welcome,
    ChoiceText,
    AdButton,
    PremiumButton,
    AlreadyProcessing,
    SubscribeRequired,
    ErrorInitDownload,
    DownloadFailed,
    DownloadTimeout,
    UnsupportedFormat,
    UploadFailed,
    LanguageChoose,
    LanguageSet,
    SendLinkGuide,
}

/// Every [`MsgKey`] variant, used by the coverage test.
pub const ALL_KEYS: &[MsgKey] = &[
    MsgKey::Welcome,
    MsgKey::ChoiceText,
    MsgKey::AdButton,
    MsgKey::PremiumButton,
    MsgKey::AlreadyProcessing,
    MsgKey::SubscribeRequired,
    MsgKey::ErrorInitDownload,
    MsgKey::DownloadFailed,
    MsgKey::DownloadTimeout,
    MsgKey::UnsupportedFormat,
    MsgKey::UploadFailed,
    MsgKey::LanguageChoose,
    MsgKey::LanguageSet,
    MsgKey::SendLinkGuide,
];

/// Language codes with full translations, as returned by [`resolve_lang`].
pub const SUPPORTED_LANGS: &[&str] = &[
    "en", "ru", "es", "ar", "zh-hans", "zh-hant", "pt-br", "tr", "uk", "id", "vi", "th", "my",
    "km", "fr", "de", "it", "nl", "ko", "ms", "hi", "bn", "fa", "pl",
];

/// (language code, button label in its own language) for the /language keyboard.
pub const LANG_BUTTONS: &[(&str, &str)] = &[
    ("en", "🇬🇧 English"),
    ("ru", "🇷🇺 Русский"),
    ("es", "🇪🇸 Español"),
    ("ar", "🇸🇦 العربية"),
    ("zh-hans", "🇨🇳 中文"),
    ("pt-br", "🇧🇷 Português"),
    ("tr", "🇹🇷 Türkçe"),
    ("uk", "🇺🇦 Українська"),
    ("id", "🇮🇩 Indonesia"),
    ("vi", "🇻🇳 Tiếng Việt"),
    ("th", "🇹🇭 ไทย"),
    ("my", "🇲🇲 မြန်မာ"),
    ("km", "🇰🇭 ខ្មែរ"),
    ("fr", "🇫🇷 Français"),
    ("de", "🇩🇪 Deutsch"),
    ("it", "🇮🇹 Italiano"),
    ("nl", "🇳🇱 Nederlands"),
    ("ko", "🇰🇷 한국어"),
    ("ms", "🇲🇾 Melayu"),
    ("hi", "🇮🇳 हिन्दी"),
    ("bn", "🇧🇩 বাংলা"),
    ("fa", "🇮🇷 فارسی"),
    ("pl", "🇵🇱 Polski"),
    ("zh-hant", "🇹🇼 繁體中文"),
];

/// True when the message text is one of the /language keyboard buttons.
pub fn is_language_button(text: &str) -> bool {
    LANG_BUTTONS.iter().any(|(_, label)| *label == text)
}

/// Language code for a /language keyboard button press.
pub fn lang_code_for_button(text: &str) -> Option<&'static str> {
    LANG_BUTTONS
        .iter()
        .find(|(_, label)| *label == text)
        .map(|(code, _)| *code)
}

/// Normalise a Telegram IETF language tag to one of [`SUPPORTED_LANGS`].
/// Exact matches win (`pt-br`, `zh-hans`), otherwise the base subtag is used
/// (`es-mx` -> `es`, `pt` -> `pt-br`, `zh` -> `zh-hans`). Anything else falls
/// back to `en`.
pub fn resolve_lang(code: Option<&str>) -> &'static str {
    let lowered = code.unwrap_or("en").to_lowercase();
    if let Some(hit) = lookup_lang(&lowered) {
        return hit;
    }
    let base = lowered.split(['-', '_']).next().unwrap_or("en");
    lookup_lang(base).unwrap_or("en")
}

fn lookup_lang(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "ru" => "ru",
        "es" => "es",
        "ar" => "ar",
        "zh-hans" | "zh" => "zh-hans",
        "zh-hant" => "zh-hant",
        "pt-br" | "pt" => "pt-br",
        "tr" => "tr",
        "uk" => "uk",
        "id" => "id",
        "vi" => "vi",
        "th" => "th",
        "my" => "my",
        "km" => "km",
        "fr" => "fr",
        "de" => "de",
        "it" => "it",
        "nl" => "nl",
        "ko" => "ko",
        "ms" => "ms",
        "hi" => "hi",
        "bn" => "bn",
        "fa" => "fa",
        "pl" => "pl",
        _ => return None,
    })
}

/// Localised bot string. Never fails: unknown languages fall back to English.
pub fn t(key: MsgKey, lang: Option<&str>) -> &'static str {
    match (key, resolve_lang(lang)) {
        // ---- Welcome ----
        (MsgKey::Welcome, "ru") => "Добро пожаловать! Отправьте мне ссылку на TikTok.",
        (MsgKey::Welcome, "es") => "¡Bienvenido! Envíame un enlace de TikTok.",
        (MsgKey::Welcome, "ar") => "مرحباً! أرسل لي رابط تيك توك.",
        (MsgKey::Welcome, "zh-hans") => "欢迎！请给我发送 TikTok 链接。",
        (MsgKey::Welcome, "zh-hant") => "歡迎！請給我發送 TikTok 連結。",
        (MsgKey::Welcome, "pt-br") => "Bem-vindo! Envie-me um link do TikTok.",
        (MsgKey::Welcome, "tr") => "Hoş geldin! Bana bir TikTok bağlantısı gönder.",
        (MsgKey::Welcome, "uk") => "Вітаю! Надішліть мені посилання на TikTok.",
        (MsgKey::Welcome, "id") => "Selamat datang! Kirimkan saya tautan TikTok.",
        (MsgKey::Welcome, "vi") => "Chào mừng! Hãy gửi cho tôi một liên kết TikTok.",
        (MsgKey::Welcome, "th") => "ยินดีต้อนรับ! ส่งลิงก์ TikTok มาให้ฉัน",
        (MsgKey::Welcome, "my") => "ကြိုဆိုပါတယ်! TikTok လင့်ခ်တစ်ခုပို့ပေးပါ။",
        (MsgKey::Welcome, "km") => "សួស្តី! សូមផ្ញើលីង TikTok មកឱ្យខ្ញុំ។",
        (MsgKey::Welcome, "fr") => "Bienvenue ! Envoyez-moi un lien TikTok.",
        (MsgKey::Welcome, "de") => "Willkommen! Sende mir einen TikTok-Link.",
        (MsgKey::Welcome, "it") => "Benvenuto! Inviami un link di TikTok.",
        (MsgKey::Welcome, "nl") => "Welkom! Stuur me een TikTok-link.",
        (MsgKey::Welcome, "ko") => "환영합니다! TikTok 링크를 보내주세요.",
        (MsgKey::Welcome, "ms") => "Selamat datang! Hantar saya pautan TikTok.",
        (MsgKey::Welcome, "hi") => "स्वागत है! मुझे एक TikTok लिंक भेजें।",
        (MsgKey::Welcome, "bn") => "স্বাগতম! আমাকে একটি TikTok লিঙ্ক পাঠান।",
        (MsgKey::Welcome, "fa") => "خوش آمدید! یک لینک تیک‌تاک برایم بفرستید.",
        (MsgKey::Welcome, "pl") => "Witaj! Wyślij mi link do TikToka.",
        (MsgKey::Welcome, _) => "Welcome! Send me a TikTok link.",
        // ---- ChoiceText ----
        (MsgKey::ChoiceText, "ru") => {
            "📥 Ваше видео готово к загрузке!\nВыберите вариант скачивания:"
        }
        (MsgKey::ChoiceText, "es") => {
            "📥 ¡Tu video está listo para descargar!\nElige una opción de descarga:"
        }
        (MsgKey::ChoiceText, "ar") => "📥 الفيديو الخاص بك جاهز للتنزيل!\nاختر خيار التنزيل:",
        (MsgKey::ChoiceText, "zh-hans") => "📥 您的视频已准备好下载！\n请选择下载选项：",
        (MsgKey::ChoiceText, "zh-hant") => "📥 您的影片已準備好下載！\n請選擇下載選項：",
        (MsgKey::ChoiceText, "pt-br") => {
            "📥 Seu vídeo está pronto para baixar!\nEscolha uma opção:"
        }
        (MsgKey::ChoiceText, "tr") => "📥 Videon indirmeye hazır!\nBir indirme seçeneği seç:",
        (MsgKey::ChoiceText, "uk") => "📥 Ваше відео готове до завантаження!\nОберіть варіант:",
        (MsgKey::ChoiceText, "id") => "📥 Video Anda siap diunduh!\nPilih opsi unduhan:",
        (MsgKey::ChoiceText, "vi") => {
            "📥 Video của bạn đã sẵn sàng để tải!\nChọn tùy chọn tải xuống:"
        }
        (MsgKey::ChoiceText, "th") => "📥 วิดีโอของคุณพร้อมดาวน์โหลดแล้ว!\nเลือกตัวเลือกการดาวน์โหลด:",
        (MsgKey::ChoiceText, "my") => "📥 သင့်ဗီဒီယိုဒေါင်းလုဒ်လုပ်ဖို့အသင့်ဖြစ်နေပါပြီ!\nဒေါင်းလုဒ်နည်းလမ်းရွေးပါ:",
        (MsgKey::ChoiceText, "km") => "📥 វីដេអូរបស់អ្នករួចរាល់សម្រាប់ទាញយកហើយ!\nជ្រើសរើសជម្រើសទាញយក:",
        (MsgKey::ChoiceText, "fr") => {
            "📥 Votre vidéo est prête à télécharger !\nChoisissez une option :"
        }
        (MsgKey::ChoiceText, "de") => {
            "📥 Dein Video ist bereit zum Herunterladen!\nWähle eine Option:"
        }
        (MsgKey::ChoiceText, "it") => "📥 Il tuo video è pronto da scaricare!\nScegli un'opzione:",
        (MsgKey::ChoiceText, "nl") => "📥 Je video is klaar om te downloaden!\nKies een optie:",
        (MsgKey::ChoiceText, "ko") => {
            "📥 동영상을 다운로드할 준비가 되었습니다!\n다운로드 옵션을 선택하세요:"
        }
        (MsgKey::ChoiceText, "ms") => {
            "📥 Video anda sedia untuk dimuat turun!\nPilih pilihan muat turun:"
        }
        (MsgKey::ChoiceText, "hi") => "📥 आपका वीडियो डाउनलोड के लिए तैयार है!\nडाउनलोड विकल्प चुनें:",
        (MsgKey::ChoiceText, "bn") => "📥 আপনার ভিডিও ডাউনলোডের জন্য প্রস্তুত!\nডাউনলোডের বিকল্প বেছে নিন:",
        (MsgKey::ChoiceText, "fa") => {
            "📥 ویدیوی شما آماده دانلود است!\nیک گزینه دانلود انتخاب کنید:"
        }
        (MsgKey::ChoiceText, "pl") => "📥 Twój film jest gotowy do pobrania!\nWybierz opcję:",
        (MsgKey::ChoiceText, _) => {
            "📥 Your video is ready for download!\nChoose a download option:"
        }
        // ---- AdButton ----
        (MsgKey::AdButton, "ru") => "🚀 Скачать видео (Бесплатно)",
        (MsgKey::AdButton, "es") => "🚀 Descargar video (Gratis)",
        (MsgKey::AdButton, "ar") => "🚀 تحميل الفيديو (مجاني)",
        (MsgKey::AdButton, "zh-hans") => "🚀 下载视频 (免费)",
        (MsgKey::AdButton, "zh-hant") => "🚀 下載影片 (免費)",
        (MsgKey::AdButton, "pt-br") => "🚀 Baixar vídeo (Grátis)",
        (MsgKey::AdButton, "tr") => "🚀 Videoyu indir (Ücretsiz)",
        (MsgKey::AdButton, "uk") => "🚀 Завантажити відео (безкоштовно)",
        (MsgKey::AdButton, "id") => "🚀 Unduh video (Gratis)",
        (MsgKey::AdButton, "vi") => "🚀 Tải video (Miễn phí)",
        (MsgKey::AdButton, "th") => "🚀 ดาวน์โหลดวิดีโอ (ฟรี)",
        (MsgKey::AdButton, "my") => "🚀 ဗီဒီယိုဒေါင်းလုဒ်လုပ်ရန် (အခမဲ့)",
        (MsgKey::AdButton, "km") => "🚀 ទាញយកវីដេអូ (ឥតគិតថ្លៃ)",
        (MsgKey::AdButton, "fr") => "🚀 Télécharger la vidéo (Gratuit)",
        (MsgKey::AdButton, "de") => "🚀 Video herunterladen (Kostenlos)",
        (MsgKey::AdButton, "it") => "🚀 Scarica il video (Gratis)",
        (MsgKey::AdButton, "nl") => "🚀 Video downloaden (Gratis)",
        (MsgKey::AdButton, "ko") => "🚀 동영상 다운로드 (무료)",
        (MsgKey::AdButton, "ms") => "🚀 Muat turun video (Percuma)",
        (MsgKey::AdButton, "hi") => "🚀 वीडियो डाउनलोड करें (मुफ़्त)",
        (MsgKey::AdButton, "bn") => "🚀 ভিডিও ডাউনলোড করুন (ফ্রি)",
        (MsgKey::AdButton, "fa") => "🚀 دانلود ویدیو (رایگان)",
        (MsgKey::AdButton, "pl") => "🚀 Pobierz film (Za darmo)",
        (MsgKey::AdButton, _) => "🚀 Download video (Free)",
        // ---- PremiumButton ----
        (MsgKey::PremiumButton, "ru") => "⭐️ Убрать рекламу (Premium)",
        (MsgKey::PremiumButton, "es") => "⭐️ Quitar anuncios (Premium)",
        (MsgKey::PremiumButton, "ar") => "⭐️ إزالة الإعلانات (Premium)",
        (MsgKey::PremiumButton, "zh-hans") => "⭐️ 移除广告 (Premium)",
        (MsgKey::PremiumButton, "zh-hant") => "⭐️ 移除廣告 (Premium)",
        (MsgKey::PremiumButton, "pt-br") => "⭐️ Remover anúncios (Premium)",
        (MsgKey::PremiumButton, "tr") => "⭐️ Reklamları kaldır (Premium)",
        (MsgKey::PremiumButton, "uk") => "⭐️ Прибрати рекламу (Premium)",
        (MsgKey::PremiumButton, "id") => "⭐️ Hapus iklan (Premium)",
        (MsgKey::PremiumButton, "vi") => "⭐️ Xóa quảng cáo (Premium)",
        (MsgKey::PremiumButton, "th") => "⭐️ ลบโฆษณา (Premium)",
        (MsgKey::PremiumButton, "my") => "⭐️ ကြော်ငြာများဖယ်ရှားရန် (Premium)",
        (MsgKey::PremiumButton, "km") => "⭐️ លុបការផ្សាយពាណិជ្ជកម្ម (Premium)",
        (MsgKey::PremiumButton, "fr") => "⭐️ Supprimer les pubs (Premium)",
        (MsgKey::PremiumButton, "de") => "⭐️ Werbung entfernen (Premium)",
        (MsgKey::PremiumButton, "it") => "⭐️ Rimuovi la pubblicità (Premium)",
        (MsgKey::PremiumButton, "nl") => "⭐️ Advertenties verwijderen (Premium)",
        (MsgKey::PremiumButton, "ko") => "⭐️ 광고 제거 (Premium)",
        (MsgKey::PremiumButton, "ms") => "⭐️ Buang iklan (Premium)",
        (MsgKey::PremiumButton, "hi") => "⭐️ विज्ञापन हटाएं (Premium)",
        (MsgKey::PremiumButton, "bn") => "⭐️ বিজ্ঞাপন সরান (Premium)",
        (MsgKey::PremiumButton, "fa") => "⭐️ حذف تبلیغات (Premium)",
        (MsgKey::PremiumButton, "pl") => "⭐️ Usuń reklamy (Premium)",
        (MsgKey::PremiumButton, _) => "⭐️ Remove ads (Premium)",
        // ---- AlreadyProcessing ----
        (MsgKey::AlreadyProcessing, "ru") => "⏳ Это видео уже обрабатывается.",
        (MsgKey::AlreadyProcessing, "es") => "⏳ Este video ya se está procesando.",
        (MsgKey::AlreadyProcessing, "ar") => "⏳ هذا الفيديو قيد المعالجة بالفعل.",
        (MsgKey::AlreadyProcessing, "zh-hans") => "⏳ 该视频正在处理中。",
        (MsgKey::AlreadyProcessing, "zh-hant") => "⏳ 該影片正在處理中。",
        (MsgKey::AlreadyProcessing, "pt-br") => "⏳ Este vídeo já está sendo processado.",
        (MsgKey::AlreadyProcessing, "tr") => "⏳ Bu video zaten işleniyor.",
        (MsgKey::AlreadyProcessing, "uk") => "⏳ Це відео вже обробляється.",
        (MsgKey::AlreadyProcessing, "id") => "⏳ Video ini sedang diproses.",
        (MsgKey::AlreadyProcessing, "vi") => "⏳ Video này đang được xử lý.",
        (MsgKey::AlreadyProcessing, "th") => "⏳ วิดีโอนี้กำลังประมวลผลอยู่",
        (MsgKey::AlreadyProcessing, "my") => "⏳ ဒီဗီဒီယိုကို လုပ်ဆောင်နေပါတယ်။",
        (MsgKey::AlreadyProcessing, "km") => "⏳ វីដេអូនេះកំពុងដំណើរការ។",
        (MsgKey::AlreadyProcessing, "fr") => "⏳ Cette vidéo est déjà en cours de traitement.",
        (MsgKey::AlreadyProcessing, "de") => "⏳ Dieses Video wird bereits verarbeitet.",
        (MsgKey::AlreadyProcessing, "it") => "⏳ Questo video è già in elaborazione.",
        (MsgKey::AlreadyProcessing, "nl") => "⏳ Deze video wordt al verwerkt.",
        (MsgKey::AlreadyProcessing, "ko") => "⏳ 이 동영상은 이미 처리 중입니다.",
        (MsgKey::AlreadyProcessing, "ms") => "⏳ Video ini sedang diproses.",
        (MsgKey::AlreadyProcessing, "hi") => "⏳ यह वीडियो पहले से प्रोसेस हो रहा है।",
        (MsgKey::AlreadyProcessing, "bn") => "⏳ এই ভিডিওটি ইতিমধ্যে প্রক্রিয়া করা হচ্ছে।",
        (MsgKey::AlreadyProcessing, "fa") => "⏳ این ویدیو در حال پردازش است.",
        (MsgKey::AlreadyProcessing, "pl") => "⏳ Ten film jest już przetwarzany.",
        (MsgKey::AlreadyProcessing, _) => "⏳ This video is already being processed.",
        // ---- SubscribeRequired ----
        (MsgKey::SubscribeRequired, "ru") => {
            "Чтобы пользоваться ботом, подпишитесь на наши каналы."
        }
        (MsgKey::SubscribeRequired, "es") => "Para usar el bot, suscríbete a nuestros canales.",
        (MsgKey::SubscribeRequired, "ar") => "لاستخدام البوت، يرجى الاشتراك في قنواتنا.",
        (MsgKey::SubscribeRequired, "zh-hans") => "要使用机器人，请订阅我们的频道。",
        (MsgKey::SubscribeRequired, "zh-hant") => "要使用機器人，請訂閱我們的頻道。",
        (MsgKey::SubscribeRequired, "pt-br") => "Para usar o bot, inscreva-se em nossos canais.",
        (MsgKey::SubscribeRequired, "tr") => "Botu kullanmak için lütfen kanallarımıza abone olun.",
        (MsgKey::SubscribeRequired, "uk") => "Щоб користуватися ботом, підпишіться на наші канали.",
        (MsgKey::SubscribeRequired, "id") => {
            "Untuk menggunakan bot, silakan berlangganan kanal kami."
        }
        (MsgKey::SubscribeRequired, "vi") => {
            "Để sử dụng bot, vui lòng đăng ký các kênh của chúng tôi."
        }
        (MsgKey::SubscribeRequired, "th") => "หากต้องการใช้บอท โปรดติดตามช่องของเรา",
        (MsgKey::SubscribeRequired, "my") => "Bot ကိုသုံးဖို့ ကျွန်ုပ်တို့ချန်နယ်တွေကို subscribe လုပ်ပေးပါ။",
        (MsgKey::SubscribeRequired, "km") => "ដើម្បីប្រើ bot សូម subscribe ឆានែលរបស់យើង។",
        (MsgKey::SubscribeRequired, "fr") => "Pour utiliser le bot, abonnez-vous à nos chaînes.",
        (MsgKey::SubscribeRequired, "de") => "Um den Bot zu nutzen, abonniere bitte unsere Kanäle.",
        (MsgKey::SubscribeRequired, "it") => "Per usare il bot, iscriviti ai nostri canali.",
        (MsgKey::SubscribeRequired, "nl") => "Abonneer je op onze kanalen om de bot te gebruiken.",
        (MsgKey::SubscribeRequired, "ko") => "봇을 사용하려면 채널을 구독해 주세요.",
        (MsgKey::SubscribeRequired, "ms") => "Untuk menggunakan bot, sila langgan saluran kami.",
        (MsgKey::SubscribeRequired, "hi") => "बॉट इस्तेमाल करने के लिए कृपया हमारे चैनलों को सब्सक्राइब करें।",
        (MsgKey::SubscribeRequired, "bn") => "বট ব্যবহার করতে, অনুগ্রহ করে আমাদের চ্যানেলগুলো সাবস্ক্রাইব করুন।",
        (MsgKey::SubscribeRequired, "fa") => "برای استفاده از ربات، لطفاً در کانال‌های ما عضو شوید.",
        (MsgKey::SubscribeRequired, "pl") => "Aby korzystać z bota, zasubskrybuj nasze kanały.",
        (MsgKey::SubscribeRequired, _) => "To use the bot, please subscribe to our channels.",
        // ---- ErrorInitDownload ----
        (MsgKey::ErrorInitDownload, "ru") => "❌ Ошибка запуска скачивания.",
        (MsgKey::ErrorInitDownload, "es") => "❌ Error al iniciar la descarga.",
        (MsgKey::ErrorInitDownload, "ar") => "❌ خطأ في بدء التنزيل.",
        (MsgKey::ErrorInitDownload, "zh-hans") => "❌ 启动下载时出错。",
        (MsgKey::ErrorInitDownload, "zh-hant") => "❌ 啟動下載時出錯。",
        (MsgKey::ErrorInitDownload, "pt-br") => "❌ Erro ao iniciar o download.",
        (MsgKey::ErrorInitDownload, "tr") => "❌ İndirme başlatılırken hata oluştu.",
        (MsgKey::ErrorInitDownload, "uk") => "❌ Помилка запуску завантаження.",
        (MsgKey::ErrorInitDownload, "id") => "❌ Gagal memulai unduhan.",
        (MsgKey::ErrorInitDownload, "vi") => "❌ Lỗi khi bắt đầu tải xuống.",
        (MsgKey::ErrorInitDownload, "th") => "❌ เกิดข้อผิดพลาดในการเริ่มดาวน์โหลด",
        (MsgKey::ErrorInitDownload, "my") => "❌ ဒေါင်းလုဒ်စတင်ရာတွင် အမှားဖြစ်နေပါတယ်။",
        (MsgKey::ErrorInitDownload, "km") => "❌ មានកំហុសក្នុងការចាប់ផ្តើមទាញយក។",
        (MsgKey::ErrorInitDownload, "fr") => "❌ Erreur lors du démarrage du téléchargement.",
        (MsgKey::ErrorInitDownload, "de") => "❌ Fehler beim Starten des Downloads.",
        (MsgKey::ErrorInitDownload, "it") => "❌ Errore nell'avvio del download.",
        (MsgKey::ErrorInitDownload, "nl") => "❌ Fout bij het starten van de download.",
        (MsgKey::ErrorInitDownload, "ko") => "❌ 다운로드 시작 중 오류가 발생했습니다.",
        (MsgKey::ErrorInitDownload, "ms") => "❌ Ralat memulakan muat turun.",
        (MsgKey::ErrorInitDownload, "hi") => "❌ डाउनलोड शुरू करने में त्रुटि।",
        (MsgKey::ErrorInitDownload, "bn") => "❌ ডাউনলোড শুরু করতে ত্রুটি।",
        (MsgKey::ErrorInitDownload, "fa") => "❌ خطا در شروع دانلود.",
        (MsgKey::ErrorInitDownload, "pl") => "❌ Błąd uruchamiania pobierania.",
        (MsgKey::ErrorInitDownload, _) => "❌ Error initializing download.",
        // ---- DownloadFailed ----
        (MsgKey::DownloadFailed, "ru") => "❌ Не удалось скачать видео. Попробуйте снова.",
        (MsgKey::DownloadFailed, "es") => "❌ No se pudo descargar el video. Inténtalo de nuevo.",
        (MsgKey::DownloadFailed, "ar") => "❌ تعذر تنزيل الفيديو. حاول مرة أخرى.",
        (MsgKey::DownloadFailed, "zh-hans") => "❌ 视频下载失败，请重试。",
        (MsgKey::DownloadFailed, "zh-hant") => "❌ 影片下載失敗，請重試。",
        (MsgKey::DownloadFailed, "pt-br") => "❌ Não foi possível baixar o vídeo. Tente novamente.",
        (MsgKey::DownloadFailed, "tr") => "❌ Video indirilemedi. Lütfen tekrar deneyin.",
        (MsgKey::DownloadFailed, "uk") => "❌ Не вдалося завантажити відео. Спробуйте ще раз.",
        (MsgKey::DownloadFailed, "id") => "❌ Gagal mengunduh video. Silakan coba lagi.",
        (MsgKey::DownloadFailed, "vi") => "❌ Không thể tải video. Vui lòng thử lại.",
        (MsgKey::DownloadFailed, "th") => "❌ ดาวน์โหลดวิดีโอไม่สำเร็จ โปรดลองอีกครั้ง",
        (MsgKey::DownloadFailed, "my") => "❌ ဗီဒီယိုဒေါင်းလုဒ်မရလိုက်ပါ။ ထပ်ကြိုးစားကြည့်ပါ။",
        (MsgKey::DownloadFailed, "km") => "❌ មិនអាចទាញយកវីដេអូបានទេ។ សូមព្យាយាមម្តងទៀត។",
        (MsgKey::DownloadFailed, "fr") => {
            "❌ Impossible de télécharger la vidéo. Veuillez réessayer."
        }
        (MsgKey::DownloadFailed, "de") => {
            "❌ Video konnte nicht heruntergeladen werden. Bitte versuche es erneut."
        }
        (MsgKey::DownloadFailed, "it") => "❌ Impossibile scaricare il video. Riprova.",
        (MsgKey::DownloadFailed, "nl") => {
            "❌ Video kon niet worden gedownload. Probeer het opnieuw."
        }
        (MsgKey::DownloadFailed, "ko") => {
            "❌ 동영상을 다운로드하지 못했습니다. 다시 시도해 주세요."
        }
        (MsgKey::DownloadFailed, "ms") => "❌ Video tidak dapat dimuat turun. Sila cuba lagi.",
        (MsgKey::DownloadFailed, "hi") => "❌ वीडियो डाउनलोड नहीं हो सका। कृपया पुनः प्रयास करें।",
        (MsgKey::DownloadFailed, "bn") => "❌ ভিডিও ডাউনলোড করা যায়নি। আবার চেষ্টা করুন।",
        (MsgKey::DownloadFailed, "fa") => "❌ دانلود ویدیو انجام نشد. لطفاً دوباره تلاش کنید.",
        (MsgKey::DownloadFailed, "pl") => "❌ Nie udało się pobrać filmu. Spróbuj ponownie.",
        (MsgKey::DownloadFailed, _) => "❌ Couldn't download the video. Please try again.",
        // ---- DownloadTimeout ----
        (MsgKey::DownloadTimeout, "ru") => "❌ Время скачивания истекло. Попробуйте снова.",
        (MsgKey::DownloadTimeout, "es") => "❌ La descarga tardó demasiado. Inténtalo de nuevo.",
        (MsgKey::DownloadTimeout, "ar") => "❌ انتهت مهلة التنزيل. حاول مرة أخرى.",
        (MsgKey::DownloadTimeout, "zh-hans") => "❌ 下载超时，请重试。",
        (MsgKey::DownloadTimeout, "zh-hant") => "❌ 下載逾時，請重試。",
        (MsgKey::DownloadTimeout, "pt-br") => "❌ O download demorou demais. Tente novamente.",
        (MsgKey::DownloadTimeout, "tr") => {
            "❌ İndirme zaman aşımına uğradı. Lütfen tekrar deneyin."
        }
        (MsgKey::DownloadTimeout, "uk") => "❌ Час завантаження вичерпано. Спробуйте ще раз.",
        (MsgKey::DownloadTimeout, "id") => "❌ Unduhan kehabisan waktu. Silakan coba lagi.",
        (MsgKey::DownloadTimeout, "vi") => "❌ Tải xuống quá thời gian. Vui lòng thử lại.",
        (MsgKey::DownloadTimeout, "th") => "❌ ดาวน์โหลดหมดเวลา โปรดลองอีกครั้ง",
        (MsgKey::DownloadTimeout, "my") => "❌ ဒေါင်းလုဒ်အချိန်ကုန်သွားပါပြီ။ ထပ်ကြိုးစားကြည့်ပါ။",
        (MsgKey::DownloadTimeout, "km") => "❌ ការទាញយកអស់ពេល។ សូមព្យាយាមម្តងទៀត។",
        (MsgKey::DownloadTimeout, "fr") => {
            "❌ Délai de téléchargement dépassé. Veuillez réessayer."
        }
        (MsgKey::DownloadTimeout, "de") => {
            "❌ Download-Zeitlimit überschritten. Bitte versuche es erneut."
        }
        (MsgKey::DownloadTimeout, "it") => "❌ Download scaduto. Riprova.",
        (MsgKey::DownloadTimeout, "nl") => "❌ Download-time-out. Probeer het opnieuw.",
        (MsgKey::DownloadTimeout, "ko") => "❌ 다운로드 시간이 초과되었습니다. 다시 시도해 주세요.",
        (MsgKey::DownloadTimeout, "ms") => "❌ Muat turun tamat masa. Sila cuba lagi.",
        (MsgKey::DownloadTimeout, "hi") => "❌ डाउनलोड का समय समाप्त। कृपया पुनः प्रयास करें।",
        (MsgKey::DownloadTimeout, "bn") => "❌ ডাউনলোডের সময় শেষ। আবার চেষ্টা করুন।",
        (MsgKey::DownloadTimeout, "fa") => "❌ زمان دانلود به پایان رسید. لطفاً دوباره تلاش کنید.",
        (MsgKey::DownloadTimeout, "pl") => "❌ Przekroczono czas pobierania. Spróbuj ponownie.",
        (MsgKey::DownloadTimeout, _) => "❌ Download timed out. Please try again.",
        // ---- UnsupportedFormat ----
        (MsgKey::UnsupportedFormat, "ru") => "❌ Этот формат ссылки не поддерживается.",
        (MsgKey::UnsupportedFormat, "es") => "❌ Este formato de enlace no es compatible.",
        (MsgKey::UnsupportedFormat, "ar") => "❌ تنسيق الرابط هذا غير مدعوم.",
        (MsgKey::UnsupportedFormat, "zh-hans") => "❌ 不支持此链接格式。",
        (MsgKey::UnsupportedFormat, "zh-hant") => "❌ 不支援此連結格式。",
        (MsgKey::UnsupportedFormat, "pt-br") => "❌ Este formato de link não é suportado.",
        (MsgKey::UnsupportedFormat, "tr") => "❌ Bu bağlantı formatı desteklenmiyor.",
        (MsgKey::UnsupportedFormat, "uk") => "❌ Цей формат посилання не підтримується.",
        (MsgKey::UnsupportedFormat, "id") => "❌ Format tautan ini tidak didukung.",
        (MsgKey::UnsupportedFormat, "vi") => "❌ Định dạng liên kết này không được hỗ trợ.",
        (MsgKey::UnsupportedFormat, "th") => "❌ ไม่รองรับรูปแบบลิงก์นี้",
        (MsgKey::UnsupportedFormat, "my") => "❌ ဒီလင့်ခ်ပုံစံကို မပံ့ပိုးထားပါ။",
        (MsgKey::UnsupportedFormat, "km") => "❌ ទម្រង់លីងនេះមិនត្រូវបានគាំទ្រទេ។",
        (MsgKey::UnsupportedFormat, "fr") => "❌ Ce format de lien n'est pas pris en charge.",
        (MsgKey::UnsupportedFormat, "de") => "❌ Dieses Link-Format wird nicht unterstützt.",
        (MsgKey::UnsupportedFormat, "it") => "❌ Questo formato di link non è supportato.",
        (MsgKey::UnsupportedFormat, "nl") => "❌ Dit linkformaat wordt niet ondersteund.",
        (MsgKey::UnsupportedFormat, "ko") => "❌ 이 링크 형식은 지원되지 않습니다.",
        (MsgKey::UnsupportedFormat, "ms") => "❌ Format pautan ini tidak disokong.",
        (MsgKey::UnsupportedFormat, "hi") => "❌ यह लिंक फ़ॉर्मेट समर्थित नहीं है।",
        (MsgKey::UnsupportedFormat, "bn") => "❌ এই লিঙ্ক ফরম্যাট সমর্থিত নয়।",
        (MsgKey::UnsupportedFormat, "fa") => "❌ این قالب لینک پشتیبانی نمی‌شود.",
        (MsgKey::UnsupportedFormat, "pl") => "❌ Ten format linku nie jest obsługiwany.",
        (MsgKey::UnsupportedFormat, _) => "❌ This link format is not supported.",
        // ---- UploadFailed ----
        (MsgKey::UploadFailed, "ru") => "❌ Не удалось отправить файл. Попробуйте снова.",
        (MsgKey::UploadFailed, "es") => "❌ Error al subir el archivo. Inténtalo de nuevo.",
        (MsgKey::UploadFailed, "ar") => "❌ فشل الرفع. حاول مرة أخرى.",
        (MsgKey::UploadFailed, "zh-hans") => "❌ 上传失败，请重试。",
        (MsgKey::UploadFailed, "zh-hant") => "❌ 上傳失敗，請重試。",
        (MsgKey::UploadFailed, "pt-br") => "❌ Falha no envio. Tente novamente.",
        (MsgKey::UploadFailed, "tr") => "❌ Yükleme başarısız oldu. Lütfen tekrar deneyin.",
        (MsgKey::UploadFailed, "uk") => "❌ Не вдалося надіслати файл. Спробуйте ще раз.",
        (MsgKey::UploadFailed, "id") => "❌ Gagal mengunggah. Silakan coba lagi.",
        (MsgKey::UploadFailed, "vi") => "❌ Tải lên thất bại. Vui lòng thử lại.",
        (MsgKey::UploadFailed, "th") => "❌ อัปโหลดไม่สำเร็จ โปรดลองอีกครั้ง",
        (MsgKey::UploadFailed, "my") => "❌ ဖိုင်ပို့မရလိုက်ပါ။ ထပ်ကြိုးစားကြည့်ပါ။",
        (MsgKey::UploadFailed, "km") => "❌ ការបង្ហោះបានបរាជ័យ។ សូមព្យាយាមម្តងទៀត។",
        (MsgKey::UploadFailed, "fr") => "❌ Échec de l'envoi. Veuillez réessayer.",
        (MsgKey::UploadFailed, "de") => "❌ Upload fehlgeschlagen. Bitte versuche es erneut.",
        (MsgKey::UploadFailed, "it") => "❌ Caricamento non riuscito. Riprova.",
        (MsgKey::UploadFailed, "nl") => "❌ Upload mislukt. Probeer het opnieuw.",
        (MsgKey::UploadFailed, "ko") => "❌ 업로드하지 못했습니다. 다시 시도해 주세요.",
        (MsgKey::UploadFailed, "ms") => "❌ Muat naik gagal. Sila cuba lagi.",
        (MsgKey::UploadFailed, "hi") => "❌ अपलोड विफल। कृपया पुनः प्रयास करें।",
        (MsgKey::UploadFailed, "bn") => "❌ আপলোড ব্যর্থ। আবার চেষ্টা করুন।",
        (MsgKey::UploadFailed, "fa") => "❌ آپلود ناموفق بود. لطفاً دوباره تلاش کنید.",
        (MsgKey::UploadFailed, "pl") => "❌ Wysyłanie nie powiodło się. Spróbuj ponownie.",
        (MsgKey::UploadFailed, _) => "❌ Upload failed. Please try again.",
        // ---- LanguageChoose ----
        (MsgKey::LanguageChoose, "ru") => "🌐 Выберите язык:",
        (MsgKey::LanguageChoose, "es") => "🌐 Elige tu idioma:",
        (MsgKey::LanguageChoose, "ar") => "🌐 اختر لغتك:",
        (MsgKey::LanguageChoose, "zh-hans") => "🌐 请选择语言：",
        (MsgKey::LanguageChoose, "zh-hant") => "🌐 請選擇語言：",
        (MsgKey::LanguageChoose, "pt-br") => "🌐 Escolha seu idioma:",
        (MsgKey::LanguageChoose, "tr") => "🌐 Dilini seç:",
        (MsgKey::LanguageChoose, "uk") => "🌐 Оберіть мову:",
        (MsgKey::LanguageChoose, "id") => "🌐 Pilih bahasamu:",
        (MsgKey::LanguageChoose, "vi") => "🌐 Chọn ngôn ngữ của bạn:",
        (MsgKey::LanguageChoose, "th") => "🌐 เลือกภาษาของคุณ:",
        (MsgKey::LanguageChoose, "my") => "🌐 သင့်ဘာသာစကားရွေးပါ:",
        (MsgKey::LanguageChoose, "km") => "🌐 ជ្រើសរើសភាសារបស់អ្នក:",
        (MsgKey::LanguageChoose, "fr") => "🌐 Choisissez votre langue :",
        (MsgKey::LanguageChoose, "de") => "🌐 Wähle deine Sprache:",
        (MsgKey::LanguageChoose, "it") => "🌐 Scegli la tua lingua:",
        (MsgKey::LanguageChoose, "nl") => "🌐 Kies je taal:",
        (MsgKey::LanguageChoose, "ko") => "🌐 언어를 선택하세요:",
        (MsgKey::LanguageChoose, "ms") => "🌐 Pilih bahasa anda:",
        (MsgKey::LanguageChoose, "hi") => "🌐 अपनी भाषा चुनें:",
        (MsgKey::LanguageChoose, "bn") => "🌐 আপনার ভাষা বেছে নিন:",
        (MsgKey::LanguageChoose, "fa") => "🌐 زبان خود را انتخاب کنید:",
        (MsgKey::LanguageChoose, "pl") => "🌐 Wybierz język:",
        (MsgKey::LanguageChoose, _) => "🌐 Choose your language:",
        // ---- LanguageSet ----
        (MsgKey::LanguageSet, "ru") => "✅ Язык обновлён.",
        (MsgKey::LanguageSet, "es") => "✅ Idioma actualizado.",
        (MsgKey::LanguageSet, "ar") => "✅ تم تحديث اللغة.",
        (MsgKey::LanguageSet, "zh-hans") => "✅ 语言已更新。",
        (MsgKey::LanguageSet, "zh-hant") => "✅ 語言已更新。",
        (MsgKey::LanguageSet, "pt-br") => "✅ Idioma atualizado.",
        (MsgKey::LanguageSet, "tr") => "✅ Dil güncellendi.",
        (MsgKey::LanguageSet, "uk") => "✅ Мову оновлено.",
        (MsgKey::LanguageSet, "id") => "✅ Bahasa diperbarui.",
        (MsgKey::LanguageSet, "vi") => "✅ Đã cập nhật ngôn ngữ.",
        (MsgKey::LanguageSet, "th") => "✅ อัปเดตภาษาแล้ว",
        (MsgKey::LanguageSet, "my") => "✅ ဘာသာစကားပြောင်းပြီးပါပြီ။",
        (MsgKey::LanguageSet, "km") => "✅ បានធ្វើបច្ចុប្បន្នភាពភាសាហើយ។",
        (MsgKey::LanguageSet, "fr") => "✅ Langue mise à jour.",
        (MsgKey::LanguageSet, "de") => "✅ Sprache aktualisiert.",
        (MsgKey::LanguageSet, "it") => "✅ Lingua aggiornata.",
        (MsgKey::LanguageSet, "nl") => "✅ Taal bijgewerkt.",
        (MsgKey::LanguageSet, "ko") => "✅ 언어가 업데이트되었습니다.",
        (MsgKey::LanguageSet, "ms") => "✅ Bahasa dikemas kini.",
        (MsgKey::LanguageSet, "hi") => "✅ भाषा अपडेट हो गई।",
        (MsgKey::LanguageSet, "bn") => "✅ ভাষা আপডেট হয়েছে।",
        (MsgKey::LanguageSet, "fa") => "✅ زبان به‌روزرسانی شد.",
        (MsgKey::LanguageSet, "pl") => "✅ Zaktualizowano język.",
        (MsgKey::LanguageSet, _) => "✅ Language updated.",
        // ---- SendLinkGuide ----
        (MsgKey::SendLinkGuide, "ru") => "🎬 Пришлите ссылку на TikTok, чтобы скачать видео, фото или музыку.",
        (MsgKey::SendLinkGuide, "es") => "🎬 Envíame un enlace de TikTok para descargar video, fotos o música.",
        (MsgKey::SendLinkGuide, "ar") => "🎬 أرسل لي رابط تيك توك لتنزيل الفيديو أو الصور أو الموسيقى.",
        (MsgKey::SendLinkGuide, "zh-hans") => "🎬 请给我发送 TikTok 链接以下载视频、照片或音乐。",
        (MsgKey::SendLinkGuide, "zh-hant") => "🎬 請給我發送 TikTok 連結以下載影片、照片或音樂。",
        (MsgKey::SendLinkGuide, "pt-br") => "🎬 Envie-me um link do TikTok para baixar vídeo, fotos ou música.",
        (MsgKey::SendLinkGuide, "tr") => "🎬 Video, fotoğraf veya müzik indirmek için bana bir TikTok bağlantısı gönder.",
        (MsgKey::SendLinkGuide, "uk") => "🎬 Надішліть посилання на TikTok, щоб завантажити відео, фото чи музику.",
        (MsgKey::SendLinkGuide, "id") => "🎬 Kirimkan saya tautan TikTok untuk mengunduh video, foto, atau musik.",
        (MsgKey::SendLinkGuide, "vi") => "🎬 Hãy gửi cho tôi một liên kết TikTok để tải video, ảnh hoặc nhạc.",
        (MsgKey::SendLinkGuide, "th") => "🎬 ส่งลิงก์ TikTok มาให้ฉันเพื่อดาวน์โหลดวิดีโอ รูปภาพ หรือเพลง",
        (MsgKey::SendLinkGuide, "my") => "🎬 ဗီဒီယို၊ ဓာတ်ပုံ ဒါမှမဟုတ် သီချင်းဒေါင်းလုဒ်လုပ်ဖို့ TikTok လင့်ခ်ပို့ပေးပါ။",
        (MsgKey::SendLinkGuide, "km") => "🎬 សូមផ្ញើលីង TikTok មកឱ្យខ្ញុំដើម្បីទាញយកវីដេអូ រូបភាព ឬតន្ត្រី។",
        (MsgKey::SendLinkGuide, "fr") => "🎬 Envoyez-moi un lien TikTok pour télécharger vidéo, photos ou musique.",
        (MsgKey::SendLinkGuide, "de") => "🎬 Sende mir einen TikTok-Link, um Videos, Fotos oder Musik herunterzuladen.",
        (MsgKey::SendLinkGuide, "it") => "🎬 Inviami un link di TikTok per scaricare video, foto o musica.",
        (MsgKey::SendLinkGuide, "nl") => "🎬 Stuur me een TikTok-link om video, foto's of muziek te downloaden.",
        (MsgKey::SendLinkGuide, "ko") => "🎬 동영상, 사진 또는 음악을 다운로드하려면 TikTok 링크를 보내주세요.",
        (MsgKey::SendLinkGuide, "ms") => "🎬 Hantar saya pautan TikTok untuk memuat turun video, foto atau muzik.",
        (MsgKey::SendLinkGuide, "hi") => "🎬 वीडियो, फोटो या संगीत डाउनलोड करने के लिए मुझे TikTok लिंक भेजें।",
        (MsgKey::SendLinkGuide, "bn") => "🎬 ভিডিও, ছবি বা গান ডাউনলোড করতে আমাকে TikTok লিঙ্ক পাঠান।",
        (MsgKey::SendLinkGuide, "fa") => "🎬 برای دانلود ویدیو، عکس یا موسیقی یک لینک تیک‌تاک برایم بفرستید.",
        (MsgKey::SendLinkGuide, "pl") => "🎬 Wyślij mi link do TikToka, aby pobrać film, zdjęcia lub muzykę.",
        (MsgKey::SendLinkGuide, _) => "🎬 Send me a TikTok link to download video, photo or music.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_exact_variants() {
        assert_eq!(resolve_lang(Some("pt-BR")), "pt-br");
        assert_eq!(resolve_lang(Some("zh-Hant")), "zh-hant");
        assert_eq!(resolve_lang(Some("zh-hans")), "zh-hans");
        assert_eq!(resolve_lang(Some("ru")), "ru");
    }

    #[test]
    fn resolve_base_fallback() {
        assert_eq!(resolve_lang(Some("pt")), "pt-br");
        assert_eq!(resolve_lang(Some("zh")), "zh-hans");
        assert_eq!(resolve_lang(Some("es-MX")), "es");
        assert_eq!(resolve_lang(Some("fr_CA")), "fr");
        assert_eq!(resolve_lang(Some("uk-UA")), "uk");
    }

    #[test]
    fn resolve_unknown_falls_back_to_english() {
        assert_eq!(resolve_lang(Some("xx")), "en");
        assert_eq!(resolve_lang(Some("nb")), "en");
        assert_eq!(resolve_lang(None), "en");
        assert_eq!(resolve_lang(Some("")), "en");
    }

    #[test]
    fn every_key_has_every_locale() {
        for &lang in SUPPORTED_LANGS {
            for &key in ALL_KEYS {
                let s = t(key, Some(lang));
                assert!(!s.is_empty(), "{key:?} has no text for {lang}");
            }
        }
    }

    #[test]
    fn language_buttons_roundtrip() {
        for &(code, label) in LANG_BUTTONS {
            assert!(is_language_button(label), "{label} not recognised");
            assert_eq!(lang_code_for_button(label), Some(code));
        }
        assert!(!is_language_button("h265"));
        assert_eq!(lang_code_for_button("h265"), None);
    }

    #[test]
    fn unknown_language_returns_english_text() {
        assert_eq!(
            t(MsgKey::DownloadFailed, Some("xx")),
            t(MsgKey::DownloadFailed, Some("en"))
        );
    }
}
