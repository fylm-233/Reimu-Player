//! 网易云分享链接 / 分享文案解析（纯函数，无网络、无 IO）。
//!
//! 为什么放在 `music` 层而不是 GUI：链接解析是**领域规则**（什么样的输入对应
//! 哪一类资源），与渲染无关；放在这里才能被单测覆盖，也符合
//! `NETEASE_INTEGRATION_SPEC.md` 3.2「UI 不解析第三方数据」的约束。
//!
//! 支持的输入形态（实测覆盖网易云 App / 网页 / 小程序分享出来的文本）：
//!
//! | 输入 | 结果 |
//! |---|---|
//! | `https://music.163.com/song?id=123456` | 歌曲 123456 |
//! | `https://music.163.com/#/song?id=123456` | 歌曲 123456 |
//! | `https://y.music.163.com/m/song?id=123456&uct2=…` | 歌曲 123456 |
//! | `https://music.163.com/playlist?id=24381616` | 歌单 24381616 |
//! | `https://music.163.com/#/my/m/music/playlist?id=24381616` | 歌单 24381616 |
//! | `https://music.163.com/album?id=32311` | 专辑 32311 |
//! | `https://music.163.com/mv?id=5436712` | MV（不支持播放，明确拒绝） |
//! | `https://music.163.com/program?id=1367665101` | 电台节目（明确拒绝） |
//! | `分享单曲《夜曲》: https://music.163.com/song?id=185811 （来自 @网易云音乐）` | 歌曲 185811 |
//! | `24381616`（纯数字） | 歌单 24381616（沿用歌单页原有语义） |
//! | `http://163cn.tv/AbCdEf`（短链） | [`ShareRef::ShortLink`]，需调用方先展开 |
//!
//! **短链必须联网展开**：解析层只识别出来，不自己发请求（保持纯函数）；
//! 展开后把最终 URL 再喂回 [`parse_share`] 即可。这一分层让"要不要联网"
//! 由调用方决定，也让解析逻辑可以在无网测试里跑。

/// 链接指向的资源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareKind {
    /// 单曲。
    Song,
    /// 歌单（含「我喜欢的音乐」）。
    Playlist,
    /// 专辑。
    Album,
}

impl ShareKind {
    /// 类型名（用于提示文案，跟界面语言）。
    ///
    /// 早先固定中文，一旦英文界面用到它就会漏出中文。
    pub fn label(&self, lang: crate::i18n::Lang) -> &'static str {
        use crate::i18n::pick;
        match self {
            ShareKind::Song => pick(lang, "单曲", "song"),
            ShareKind::Playlist => pick(lang, "歌单", "playlist"),
            ShareKind::Album => pick(lang, "专辑", "album"),
        }
    }
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareRef {
    /// 已解析出可用的资源引用。
    Resolved {
        /// 资源类型。
        kind: ShareKind,
        /// 资源 ID（纯数字串，已去掉前导空白）。
        id: String,
    },
    /// 短链：需要调用方联网展开后再解析一次。
    ShortLink(String),
    /// 是网易云链接，但类型不支持（MV / 电台 / 数字专辑等）。
    Unsupported {
        /// 原始路径段，用于提示（如 `mv`）。
        segment: String,
    },
}

/// 解析分享链接或分享文案。
///
/// 返回 `None` 表示输入里**找不到任何网易云链接/ID**（不是"解析失败"，
/// 而是"这不是一条分享内容"），调用方据此提示用户粘贴完整链接。
///
/// 解析顺序：
/// 1. 先在整个字符串里找 `http(s)://` 开头的候选（分享文案里链接前面常带文字）；
/// 2. 逐个候选判断域名：`163cn.tv` → 短链；`music.163.com` / `y.music.163.com`
///    / `music.163.com` 的移动端域名 → 继续解析路径 + 查询参数；
/// 3. 都不匹配时，若整个输入是纯数字，按歌单 ID 处理（兼容歌单页原有输入框）。
pub fn parse_share(input: &str) -> Option<ShareRef> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }

    for url in extract_urls(text) {
        if let Some(hit) = parse_url(&url) {
            return Some(hit);
        }
    }

    // 纯数字兜底：歌单页的历史输入就是裸 ID
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) {
        return Some(ShareRef::Resolved {
            kind: ShareKind::Playlist,
            id: text.to_string(),
        });
    }
    None
}

/// 从任意文本里切出所有 `http(s)://…` 候选（到空白或中文标点为止）。
///
/// 不做完整 URL 解析：分享文案里的链接常被中文括号/书名号包住，
/// 只要切到"不可能出现在 URL 里"的字符即可。
fn extract_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        let lower = rest.to_ascii_lowercase();
        let at = lower
            .find("http://")
            .into_iter()
            .chain(lower.find("https://"))
            .min();
        match at {
            Some(off) => {
                let start = i + off;
                let tail = &text[start..];
                let end = tail
                    .char_indices()
                    .find(|(_, c)| {
                        c.is_whitespace()
                            || matches!(
                                c,
                                '（' | '）' | '《' | '》' | '“' | '”' | '，' | '。' | '"' | '\''
                            )
                    })
                    .map(|(idx, _)| idx)
                    .unwrap_or(tail.len());
                out.push(tail[..end].to_string());
                i = start + end;
            }
            None => break,
        }
    }
    out
}

/// 解析单条 URL。
fn parse_url(url: &str) -> Option<ShareRef> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let host = host.to_ascii_lowercase();

    // 短链：必须联网展开
    if host == "163cn.tv" {
        let path = path_and_query.trim_start_matches('/');
        if path.is_empty() {
            return None;
        }
        return Some(ShareRef::ShortLink(url.to_string()));
    }

    // 只认网易云自家域名；其他站点视为"不是分享内容"
    let is_netease = host == "music.163.com"
        || host == "y.music.163.com"
        || host.ends_with(".music.163.com")
        || host == "music.163.com.cn";
    if !is_netease {
        return None;
    }

    // 路径里可能带 `#/`（前端路由）：统一拆成 path + query
    let (path, query) = split_path_query(path_and_query);
    let segment = path
        .split('/')
        .rfind(|s| !s.is_empty() && !s.starts_with('#'))
        .unwrap_or("")
        .to_ascii_lowercase();

    let id = query_param(&query, "id")?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    match segment.as_str() {
        "song" => Some(ShareRef::Resolved {
            kind: ShareKind::Song,
            id,
        }),
        "playlist" => Some(ShareRef::Resolved {
            kind: ShareKind::Playlist,
            id,
        }),
        "album" => Some(ShareRef::Resolved {
            kind: ShareKind::Album,
            id,
        }),
        "" => None,
        other => Some(ShareRef::Unsupported {
            segment: other.to_string(),
        }),
    }
}

/// 拆出路径与查询串（同时处理 `#/song?id=1` 这种 hash 路由）。
fn split_path_query(s: &str) -> (String, String) {
    let s = s.strip_prefix('/').unwrap_or(s);
    let s = s.strip_prefix('#').unwrap_or(s);
    let s = s.strip_prefix('/').unwrap_or(s);
    match s.find('?') {
        Some(idx) => (s[..idx].to_string(), s[idx + 1..].to_string()),
        None => (s.to_string(), String::new()),
    }
}

/// 取查询参数（`a=1&b=2`，容忍 `#` 之后的片段与重复 key）。
fn query_param(query: &str, key: &str) -> Option<String> {
    let query = query.split('#').next().unwrap_or(query);
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        let v = it.next().unwrap_or("");
        if k.eq_ignore_ascii_case(key) {
            return Some(v.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(kind: ShareKind, id: &str) -> Option<ShareRef> {
        Some(ShareRef::Resolved {
            kind,
            id: id.to_string(),
        })
    }

    #[test]
    fn parses_song_url_variants() {
        for url in [
            "https://music.163.com/song?id=185811",
            "https://music.163.com/#/song?id=185811",
            "https://y.music.163.com/m/song?id=185811&uct2=abc",
            "http://music.163.com/song?id=185811&userid=1",
            "https://music.163.com/m/song/185811",
        ] {
            // 最后一条是旧式路径 ID，没有查询串 —— 明确不支持（返回 None）
            let want = if url.contains("song/185811") {
                None
            } else {
                resolved(ShareKind::Song, "185811")
            };
            assert_eq!(parse_share(url), want, "url={url}");
        }
    }

    #[test]
    fn parses_playlist_and_album() {
        assert_eq!(
            parse_share("https://music.163.com/playlist?id=24381616"),
            resolved(ShareKind::Playlist, "24381616")
        );
        assert_eq!(
            parse_share("https://music.163.com/#/my/m/music/playlist?id=24381616"),
            resolved(ShareKind::Playlist, "24381616")
        );
        assert_eq!(
            parse_share("https://music.163.com/album?id=32311"),
            resolved(ShareKind::Album, "32311")
        );
    }

    #[test]
    fn parses_link_inside_share_text() {
        let text = "分享单曲《夜曲》: https://music.163.com/song?id=185811 （来自 @网易云音乐）";
        assert_eq!(parse_share(text), resolved(ShareKind::Song, "185811"));

        let text = "分享歌单「City Pop 夜间驾驶」https://music.163.com/playlist?id=24381616";
        assert_eq!(parse_share(text), resolved(ShareKind::Playlist, "24381616"));
    }

    #[test]
    fn detects_short_link_for_caller_to_expand() {
        // 短链不联网：只识别，交给调用方展开
        assert_eq!(
            parse_share("http://163cn.tv/AbCdEf"),
            Some(ShareRef::ShortLink("http://163cn.tv/AbCdEf".into()))
        );
    }

    #[test]
    fn rejects_unsupported_netease_resources() {
        assert_eq!(
            parse_share("https://music.163.com/mv?id=5436712"),
            Some(ShareRef::Unsupported {
                segment: "mv".into()
            })
        );
        assert_eq!(
            parse_share("https://music.163.com/program?id=1367665101"),
            Some(ShareRef::Unsupported {
                segment: "program".into()
            })
        );
    }

    #[test]
    fn bare_digits_are_playlist_ids() {
        // 歌单页历史输入就是裸 ID，必须继续可用
        assert_eq!(
            parse_share("24381616"),
            resolved(ShareKind::Playlist, "24381616")
        );
        assert_eq!(
            parse_share("  24381616  "),
            resolved(ShareKind::Playlist, "24381616")
        );
    }

    #[test]
    fn non_netease_or_empty_input_is_none() {
        assert_eq!(parse_share(""), None);
        assert_eq!(parse_share("   "), None);
        assert_eq!(parse_share("夜曲 周杰伦"), None);
        assert_eq!(parse_share("https://example.com/song?id=1"), None);
        // 网易云链接但缺 id
        assert_eq!(parse_share("https://music.163.com/song"), None);
        // id 非数字
        assert_eq!(parse_share("https://music.163.com/song?id=abc"), None);
    }

    #[test]
    fn extracts_multiple_urls_and_takes_first_netease() {
        let text = "看看这个 https://example.com/x 还有 https://music.163.com/song?id=7";
        assert_eq!(parse_share(text), resolved(ShareKind::Song, "7"));
    }

    #[test]
    fn query_param_is_case_insensitive_and_stops_at_hash() {
        assert_eq!(query_param("ID=5&b=2", "id").as_deref(), Some("5"));
        assert_eq!(query_param("id=5#frag", "id").as_deref(), Some("5"));
        assert_eq!(query_param("x=1", "id"), None);
    }
}
