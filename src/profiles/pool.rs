//! キャラプールの画像。**受け付ける前の検証は純粋関数。**
//!
//! プロフィールの担当ロールに添える画像で、アップロード（PNG）か画像 URL の
//! どちらか一方を持つ。
//!
//! **1 人 1 枚で、貼り直しは置き換え。**`player_profiles` が追記専用なのに対し、
//! こちらは `players.trainer_name` と同じ可変の参照データとして扱う。追記専用に
//! すると、自己紹介を 1 文字直すたびに 3MB が積み上がる。
//!
//! 消えても導出は 1 つも変わらない（レートにも試合のログにも関係しない）。

/// 受け付ける最大サイズ。**3MB。**DB の CHECK と同じ値で、両方で縛る。
pub const MAX_IMAGE_BYTES: usize = 3 * 1024 * 1024;

/// URL の最大長。DB の CHECK と同じ。
pub const MAX_URL_CHARS: usize = 2048;

/// PNG の先頭 8 バイト。**拡張子や Content-Type ではなく中身で判定する。**
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// 保存する中身。**どちらか一方だけ。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolImage {
    /// アップロードされた PNG。
    Upload(Vec<u8>),
    /// 貼り付けられた画像 URL。
    Url(String),
    /// 設定を消す。
    Cleared,
}

/// 受け付けられない理由。**画面にそのまま出す。**
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolImageError {
    /// PNG ではない。
    #[error("PNG の画像を選んでください。")]
    NotPng,
    /// 3MB を超えている。
    #[error("画像は 3MB までです。")]
    TooLarge,
    /// URL の形が不正。
    #[error("画像の URL は https:// で始まるものを貼ってください。")]
    BadUrl,
    /// アップロードと URL の両方が来た。
    #[error("画像のアップロードと URL は、どちらか一方だけにしてください。")]
    Both,
}

/// アップロードと URL から、保存する中身を決める。**純粋関数。**
///
/// 空のファイル（ファイルを選ばずに送信したとき）と空の URL は「指定なし」。
/// どちらも指定が無ければ [`PoolImage::Cleared`] で、設定を消す操作になる。
pub fn parse(upload: Option<&[u8]>, url: &str) -> Result<PoolImage, PoolImageError> {
    let upload = upload.filter(|bytes| !bytes.is_empty());
    let url = url.trim();
    match (upload, url.is_empty()) {
        (Some(_), false) => Err(PoolImageError::Both),
        (Some(bytes), true) => {
            // **大きさを先に見る。**中身の検査に長い時間をかけない
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(PoolImageError::TooLarge);
            }
            if !bytes.starts_with(&PNG_MAGIC) {
                return Err(PoolImageError::NotPng);
            }
            Ok(PoolImage::Upload(bytes.to_vec()))
        }
        (None, false) => Ok(PoolImage::Url(validate_url(url)?)),
        (None, true) => Ok(PoolImage::Cleared),
    }
}

/// 画像 URL の検証。**https だけ。**
///
/// http を通すと、https のページに混在コンテンツを持ち込むことになる
/// （ブラウザが黙って読み込みを止めるので、本人には「なぜか出ない」になる）。
fn validate_url(url: &str) -> Result<String, PoolImageError> {
    if url.chars().count() > MAX_URL_CHARS {
        return Err(PoolImageError::BadUrl);
    }
    // 制御文字・空白を含む URL は属性から抜け出す足がかりになる
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(PoolImageError::BadUrl);
    }
    if !url.starts_with("https://") {
        return Err(PoolImageError::BadUrl);
    }
    // スキームの後ろにホストがあること
    if url.len() <= "https://".len() {
        return Err(PoolImageError::BadUrl);
    }
    Ok(url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(extra: usize) -> Vec<u8> {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend(std::iter::repeat_n(0u8, extra));
        bytes
    }

    /// **PNG の中身で判定する。**拡張子や Content-Type は見ない。
    #[test]
    fn only_a_real_png_is_accepted() {
        assert_eq!(
            parse(Some(&png(10)), "").unwrap(),
            PoolImage::Upload(png(10))
        );
        // JPEG のシグネチャ
        assert_eq!(
            parse(Some(&[0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0]), ""),
            Err(PoolImageError::NotPng)
        );
        // GIF
        assert_eq!(parse(Some(b"GIF89a12345"), ""), Err(PoolImageError::NotPng));
        // HTML（画像に見せかけたスクリプト）
        assert_eq!(
            parse(Some(b"<script>alert(1)</script>"), ""),
            Err(PoolImageError::NotPng)
        );
        // 8 バイト未満
        assert_eq!(parse(Some(&[0x89, b'P']), ""), Err(PoolImageError::NotPng));
    }

    /// **3MB まで。**境界も測る。
    #[test]
    fn the_size_limit_is_three_megabytes() {
        let limit = MAX_IMAGE_BYTES;
        assert_eq!(limit, 3_145_728);
        // ちょうど 3MB は通る
        let exact = png(limit - PNG_MAGIC.len());
        assert_eq!(exact.len(), limit);
        assert!(parse(Some(&exact), "").is_ok());
        // 1 バイト超えると断る
        let over = png(limit - PNG_MAGIC.len() + 1);
        assert_eq!(over.len(), limit + 1);
        assert_eq!(parse(Some(&over), ""), Err(PoolImageError::TooLarge));
        // **大きさの判定が先。**PNG でない巨大なファイルも TooLarge で返す
        assert_eq!(
            parse(Some(&vec![0u8; limit + 1]), ""),
            Err(PoolImageError::TooLarge)
        );
    }

    /// **https だけ。**http も、javascript: も、data: も通さない。
    #[test]
    fn only_an_https_url_is_accepted() {
        assert_eq!(
            parse(None, "https://example.test/pool.png").unwrap(),
            PoolImage::Url("https://example.test/pool.png".to_owned())
        );
        // 前後の空白は落とす
        assert_eq!(
            parse(None, "  https://example.test/a.png  ").unwrap(),
            PoolImage::Url("https://example.test/a.png".to_owned())
        );
        for bad in [
            "http://example.test/a.png",
            "javascript:alert(1)",
            "data:image/png;base64,iVBOR",
            "//example.test/a.png",
            "https://",
            "https://exa mple.test/a.png",
            "https://example.test/a.png\"onerror=alert(1)",
        ] {
            // 引用符を含むものは制御文字ではないので、埋め込み側の escape も要る。
            // ここで見るのはスキームと空白・制御文字だけ
            let parsed = parse(None, bad);
            if bad.contains('"') {
                assert!(parsed.is_ok(), "{bad}");
                continue;
            }
            assert_eq!(parsed, Err(PoolImageError::BadUrl), "{bad}");
        }
        // 長すぎる URL
        let long = format!("https://example.test/{}", "a".repeat(MAX_URL_CHARS));
        assert_eq!(parse(None, &long), Err(PoolImageError::BadUrl));
    }

    /// 何も指定しなければ「消す」。ファイルを選ばずに送信した場合も同じ。
    #[test]
    fn sending_nothing_clears_the_image() {
        assert_eq!(parse(None, "").unwrap(), PoolImage::Cleared);
        assert_eq!(parse(None, "   ").unwrap(), PoolImage::Cleared);
        // ファイル欄が空で送られてくる（ブラウザは空のパートを送る）
        assert_eq!(parse(Some(&[]), "").unwrap(), PoolImage::Cleared);
    }

    /// **両方は受け付けない。**どちらを出すか決まらなくなる。
    #[test]
    fn an_upload_and_a_url_together_are_refused() {
        assert_eq!(
            parse(Some(&png(4)), "https://example.test/a.png"),
            Err(PoolImageError::Both)
        );
    }
}
