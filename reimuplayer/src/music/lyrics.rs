//! 歌词模型与容错解析（规范 7.4）。
//!
//! 设计约束（`NETEASE_INTEGRATION_SPEC.md` 7.4）：
//! - 普通歌词与逐字歌词**必须区分**（[`LyricKind`]）。
//! - 歌词解析必须处理时间戳缺失和格式错误——坏行丢弃并计数，不致整体失败。
//! - 歌词缺失不得影响播放——解析产物允许为空，由调用方仅做提示。
//! - 歌词同步使用**实际播放时钟**（调用方传入 [`std::time::Duration`] 播放位置），
//!   本模块只做纯内存二分查询，不做任何 I/O。
//!
//! 支持的格式：
//! - 普通歌词：LRC（`[mm:ss.xx]文本`，一行可带多个时间戳；`[offset:±ms]` 标签）。
//! - 逐字歌词：网易云 yrc 风格（`[start,end](start,dur,0)词(start,dur,0)词…`，
//!   毫秒绝对时间戳），逐字解析失败由上层回退普通歌词。

use std::time::Duration;

/// 歌词种类：逐字与普通必须区分（规范 7.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricKind {
    /// 逐字歌词（yrc 风格，毫秒时间戳）。
    WordByWord,
    /// 普通歌词（LRC，行级时间戳）。
    Plain,
}

impl std::fmt::Display for LyricKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LyricKind::WordByWord => write!(f, "逐字歌词"),
            LyricKind::Plain => write!(f, "普通歌词"),
        }
    }
}

/// 一行歌词：时间戳（毫秒，已含 offset 修正）+ 文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    /// 触发时刻（毫秒，相对歌曲起点，已应用 [`Lyrics::offset_ms`] 修正）。
    pub start_ms: u64,
    pub text: String,
}

/// 解析完成的歌词。
///
/// 时间戳已升序排序；`dropped_lines` 记录因格式问题丢弃的行数（诊断用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lyrics {
    pub kind: LyricKind,
    pub lines: Vec<LyricLine>,
    /// `[offset:±ms]` 标签（毫秒，正数=歌词提前）。已在各行时间戳上应用。
    pub offset_ms: i64,
    /// 解析中丢弃的坏行数量（时间戳缺失/格式错误）。
    pub dropped_lines: usize,
}

impl Lyrics {
    /// 空歌词（无歌词/纯音乐）。
    pub fn empty(kind: LyricKind) -> Self {
        Self {
            kind,
            lines: Vec::new(),
            offset_ms: 0,
            dropped_lines: 0,
        }
    }

    /// 是否没有有效歌词行。
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 当前播放位置应显示的歌词行下标（实际播放时钟驱动，规范 7.4）。
    ///
    /// 返回最后一行 `start_ms <= position` 的下标；位置早于第一行或无歌词返回 `None`。
    /// 纯内存二分，可在 UI/同步循环中高频调用。
    pub fn line_at(&self, position: Duration) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        let pos_ms = position.as_millis() as u64;
        // 找最后一个 start_ms <= pos_ms 的下标
        match self.lines.partition_point(|l| l.start_ms <= pos_ms) {
            0 => None,
            n => Some(n - 1),
        }
    }
}

/// 解析 LRC 普通歌词。
///
/// 容错规则（规范 7.4「必须处理时间戳缺失和格式错误」）：
/// - `[ar:]`/`[ti:]`/`[offset:]` 等标签行不产出歌词行；`[offset:]` 提取为偏移。
/// - 一行多个时间戳展开为多行。
/// - 无时间戳行、时间戳格式错误行**丢弃并计数**，不致整体失败。
/// - 文本去除首尾空白；展开后文本为空的行保留（纯音乐常见空行占位）。
pub fn parse_lrc(text: &str) -> Lyrics {
    let mut raw: Vec<(i64, String)> = Vec::new(); // (未修正 start_ms, text)
    let mut offset_ms: i64 = 0;
    let mut dropped = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 收集行首全部 [tag]；行内允许 [t1][t2]text
        let mut rest = line;
        let mut stamps: Vec<i64> = Vec::new();
        let mut bad_stamp = false;
        while let Some(open) = rest.find('[') {
            let Some(close) = rest.find(']') else {
                break;
            };
            if close < open {
                break;
            }
            let tag = &rest[open + 1..close];
            match parse_lrc_tag(tag) {
                LrcTag::Time(ms) => stamps.push(ms),
                LrcTag::Offset(ms) => offset_ms = ms,
                LrcTag::Meta => {}
                LrcTag::Invalid => {
                    bad_stamp = true;
                    break;
                }
            }
            rest = &rest[close + 1..];
        }
        if bad_stamp && stamps.is_empty() {
            dropped += 1;
            continue;
        }
        if stamps.is_empty() {
            // 无时间戳：可能是纯文本歌词行（整篇无时间轴）——丢弃并计数，
            // 保持「带时间轴才能同步」的不变式（规范 7.4 同步语义）。
            dropped += 1;
            continue;
        }
        let content = rest.trim().to_string();
        for ms in stamps {
            raw.push((ms, content.clone()));
        }
    }

    finish(LyricKind::Plain, raw, offset_ms, dropped)
}

/// LRC 标签分类。
enum LrcTag {
    /// `[mm:ss.xx]` / `[mm:ss.xxx]` / `[mm:ss]` → 歌词时间戳（毫秒）。
    Time(i64),
    /// `[offset:±ms]`。
    Offset(i64),
    /// `[ar:]`/`[ti:]`/`[al:]`/`[by:]` 等元信息标签，忽略。
    Meta,
    /// 形似标签但不可解析（如 `[xx:yy]` 非法数字）。
    Invalid,
}

fn parse_lrc_tag(tag: &str) -> LrcTag {
    // offset 标签
    if let Some(v) = tag.strip_prefix("offset:") {
        return match v.trim().parse::<i64>() {
            Ok(ms) => LrcTag::Offset(ms),
            Err(_) => LrcTag::Invalid,
        };
    }
    // 已知元信息标签（不产出歌词行）
    for meta in ["ar:", "ti:", "al:", "by:", "re:", "ve:", "length:", "tool:"] {
        if tag.starts_with(meta) {
            return LrcTag::Meta;
        }
    }
    // 时间戳 mm:ss(.xx|.xxx)?
    let Some((mm, ss)) = tag.split_once(':') else {
        return LrcTag::Invalid;
    };
    let Ok(minutes) = mm.trim().parse::<i64>() else {
        return LrcTag::Invalid;
    };
    let (sec_part, frac_part) = match ss.split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (ss, None),
    };
    let Ok(seconds) = sec_part.parse::<i64>() else {
        return LrcTag::Invalid;
    };
    let frac_ms = match frac_part {
        // .x → 100ms，.xx → 10ms 精度，.xxx → 1ms
        Some(f) if !f.is_empty() && f.len() <= 3 && f.bytes().all(|b| b.is_ascii_digit()) => {
            let v: i64 = f.parse().unwrap_or(0);
            v * 10i64.pow(3 - f.len() as u32)
        }
        Some(_) => return LrcTag::Invalid,
        None => 0,
    };
    if minutes < 0 || seconds < 0 || seconds > 59 {
        return LrcTag::Invalid;
    }
    LrcTag::Time(minutes * 60_000 + seconds * 1_000 + frac_ms)
}

/// 解析逐字歌词（网易云 yrc 风格）。
///
/// 形如：`[1810,3400](1810,280,0)词(2090,280,0)词…` 或整行 `[start,end]文本`。
/// 容错规则同 [`parse_lrc`]：坏行丢弃并计数；行时间戳取 `[start,end]` 的 `start`；
/// 行文本为逐字段文本的拼接（无逐字段时取剩余原文）。
pub fn parse_yrc(text: &str) -> Lyrics {
    let mut raw: Vec<(i64, String)> = Vec::new();
    let mut dropped = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 行首 [start,end]
        let Some(end_bracket) = line.find(']') else {
            dropped += 1;
            continue;
        };
        let header = &line[1..end_bracket];
        let Some((start_s, _end_s)) = header.split_once(',') else {
            dropped += 1;
            continue;
        };
        let Ok(start_ms) = start_s.trim().parse::<i64>() else {
            dropped += 1;
            continue;
        };
        if start_ms < 0 {
            dropped += 1;
            continue;
        }

        let body = &line[end_bracket + 1..];
        // 逐字段 (start,dur,0)文本
        let mut content = String::new();
        let mut cursor = 0usize;
        let mut saw_word = false;
        while let Some(open) = body[cursor..].find('(') {
            let open = cursor + open;
            let Some(close) = body[open..].find(')') else {
                break;
            };
            let close = open + close;
            let header = &body[open + 1..close];
            // 词文本：到下一个 '(' 之前
            let next_open = body[close + 1..]
                .find('(')
                .map(|i| close + 1 + i)
                .unwrap_or(body.len());
            let word = body[close + 1..next_open].trim();
            // 逐字段头应是 start,dur,flag
            let parts: Vec<&str> = header.split(',').collect();
            if parts.len() >= 2
                && parts[0].parse::<i64>().is_ok()
                && parts[1].parse::<i64>().is_ok()
            {
                saw_word = true;
                content.push_str(word);
            }
            cursor = next_open;
        }
        if !saw_word {
            // 无逐字段：整段 body 即行文本
            content = body.trim().to_string();
        }
        raw.push((start_ms, content));
    }

    // 逐字歌词使用绝对毫秒时间戳，无 LRC 式 offset 标签
    finish(LyricKind::WordByWord, raw, 0, dropped)
}

/// 排序、应用 offset、组装 [`Lyrics`]。
fn finish(kind: LyricKind, mut raw: Vec<(i64, String)>, offset_ms: i64, dropped: usize) -> Lyrics {
    // offset 语义（LRC 惯例）：正数表示歌词提前 → 时间戳减去 offset
    raw.retain_mut(|(ms, _)| {
        *ms -= offset_ms;
        // 修正后为负的时间戳钳到 0（提前于歌曲起点的行在 0ms 触发）
        if *ms < 0 {
            *ms = 0;
        }
        true
    });
    raw.sort_by_key(|(ms, _)| *ms);
    Lyrics {
        kind,
        lines: raw
            .into_iter()
            .map(|(ms, text)| LyricLine {
                start_ms: ms as u64,
                text,
            })
            .collect(),
        offset_ms,
        dropped_lines: dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    // ---- LRC ----

    #[test]
    fn lrc_parses_timestamps_and_text() {
        let lrc = "[00:10.00]第一行\n[00:20.50]第二行\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.kind, LyricKind::Plain);
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(
            lyrics.lines[0],
            LyricLine {
                start_ms: 10_000,
                text: "第一行".into()
            }
        );
        assert_eq!(
            lyrics.lines[1],
            LyricLine {
                start_ms: 20_500,
                text: "第二行".into()
            }
        );
        assert_eq!(lyrics.dropped_lines, 0);
    }

    #[test]
    fn lrc_millisecond_precision_variants() {
        let lrc = "[00:01.5]a\n[00:02.50]b\n[00:03.500]c\n[00:04]d\n";
        let lyrics = parse_lrc(lrc);
        let starts: Vec<u64> = lyrics.lines.iter().map(|l| l.start_ms).collect();
        assert_eq!(starts, vec![1_500, 2_500, 3_500, 4_000]);
    }

    #[test]
    fn lrc_multi_timestamp_expands_to_multiple_lines() {
        let lrc = "[00:01.00][00:05.00]副歌\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].start_ms, 1_000);
        assert_eq!(lyrics.lines[1].start_ms, 5_000);
        assert_eq!(lyrics.lines[0].text, "副歌");
    }

    #[test]
    fn lrc_meta_tags_do_not_produce_lines() {
        let lrc = "[ar:歌手]\n[ti:标题]\n[00:01.00]正文\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "正文");
    }

    #[test]
    fn lrc_bad_lines_are_dropped_not_fatal() {
        let lrc = "没有时间戳的行\n[xx:yy]坏时间戳\n[00:01.00]好行\n[not-a-tag\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "好行");
        assert!(lyrics.dropped_lines >= 2, "坏行应被计数丢弃: {:?}", lyrics);
    }

    #[test]
    fn lrc_offset_tag_shifts_timestamps() {
        let lrc = "[offset:500]\n[00:02.00]行\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.offset_ms, 500);
        // 正 offset = 歌词提前 → 2000-500
        assert_eq!(lyrics.lines[0].start_ms, 1_500);
    }

    #[test]
    fn lrc_offset_clamps_negative_to_zero() {
        let lrc = "[offset:2000]\n[00:01.00]行\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.lines[0].start_ms, 0);
    }

    #[test]
    fn lrc_lines_sorted_regardless_of_input_order() {
        let lrc = "[00:30.00]后\n[00:10.00]前\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.lines[0].text, "前");
        assert_eq!(lyrics.lines[1].text, "后");
    }

    #[test]
    fn lrc_empty_input_yields_empty_lyrics() {
        let lyrics = parse_lrc("");
        assert!(lyrics.is_empty());
        assert_eq!(lyrics.line_at(ms(0)), None);
    }

    // ---- yrc（逐字）----

    #[test]
    fn yrc_parses_word_segments_and_line_start() {
        let yrc = "[1810,3400](1810,280,0)词(2090,280,0)句\n[4000,5000](4000,100,0)二\n";
        let lyrics = parse_yrc(yrc);
        assert_eq!(lyrics.kind, LyricKind::WordByWord);
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].start_ms, 1_810);
        assert_eq!(lyrics.lines[0].text, "词句");
        assert_eq!(lyrics.lines[1].text, "二");
    }

    #[test]
    fn yrc_line_without_word_segments_falls_back_to_body_text() {
        let yrc = "[100,200]整行文本\n";
        let lyrics = parse_yrc(yrc);
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "整行文本");
    }

    #[test]
    fn yrc_bad_lines_are_dropped_not_fatal() {
        let yrc = "没有括号\n[,500]缺start\n[-5,500]负时间\n[100,200](100,50,0)好\n";
        let lyrics = parse_yrc(yrc);
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "好");
        assert_eq!(lyrics.dropped_lines, 3);
    }

    // ---- 同步查询 line_at ----

    #[test]
    fn line_at_follows_playback_position() {
        let lrc = "[00:01.00]一\n[00:02.00]二\n[00:03.00]三\n";
        let lyrics = parse_lrc(lrc);
        assert_eq!(lyrics.line_at(ms(0)), None, "早于第一行无歌词");
        assert_eq!(lyrics.line_at(ms(1_000)), Some(0));
        assert_eq!(lyrics.line_at(ms(1_500)), Some(0));
        assert_eq!(lyrics.line_at(ms(2_000)), Some(1));
        assert_eq!(lyrics.line_at(ms(9_999)), Some(2), "超出最后一行停在末行");
    }

    #[test]
    fn line_at_empty_lyrics_returns_none() {
        let lyrics = Lyrics::empty(LyricKind::Plain);
        assert_eq!(lyrics.line_at(ms(5_000)), None);
    }
}
