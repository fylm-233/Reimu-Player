//! 导出：token 对照表 / 主题矩阵（供设计核对与快照测试，非业务渲染路径）。
//!
//! 只产出**确定性字符串**（十六进制 / TSV），不依赖任何 UI 框架。

use super::ramp::ANCHORS;
use super::semantic::{build_tokens, default_tokens, Accent};

/// 一个语义 token 的行：名称 + Light/Dark 十六进制 + 用途说明。
pub struct TokenRow {
    pub name: &'static str,
    pub light: String,
    pub dark: String,
    pub usage: &'static str,
}

/// 生成默认主题（靛蓝）的 token 对照表（供 `TOKENS.md` 核对）。
pub fn token_table() -> Vec<TokenRow> {
    let t = default_tokens();
    let rows: Vec<TokenRow> = vec![
        TokenRow {
            name: "bg",
            light: t.bg.light.to_hex(),
            dark: t.bg.dark.to_hex(),
            usage: "页面底",
        },
        TokenRow {
            name: "surface",
            light: t.surface.light.to_hex(),
            dark: t.surface.dark.to_hex(),
            usage: "主卡片",
        },
        TokenRow {
            name: "surface_raised",
            light: t.surface_raised.light.to_hex(),
            dark: t.surface_raised.dark.to_hex(),
            usage: "列表行/弹层",
        },
        TokenRow {
            name: "surface_sunken",
            light: t.surface_sunken.light.to_hex(),
            dark: t.surface_sunken.dark.to_hex(),
            usage: "歌词/波形区",
        },
        TokenRow {
            name: "line_strong",
            light: t.line_strong.light.to_hex(),
            dark: t.line_strong.dark.to_hex(),
            usage: "硬描边",
        },
        TokenRow {
            name: "line",
            light: t.line.light.to_hex(),
            dark: t.line.dark.to_hex(),
            usage: "常规分隔",
        },
        TokenRow {
            name: "line_subtle",
            light: t.line_subtle.light.to_hex(),
            dark: t.line_subtle.dark.to_hex(),
            usage: "行内分隔",
        },
        TokenRow {
            name: "text",
            light: t.text.light.to_hex(),
            dark: t.text.dark.to_hex(),
            usage: "标题/歌名",
        },
        TokenRow {
            name: "text_secondary",
            light: t.text_secondary.light.to_hex(),
            dark: t.text_secondary.dark.to_hex(),
            usage: "次要信息",
        },
        TokenRow {
            name: "text_tertiary",
            light: t.text_tertiary.light.to_hex(),
            dark: t.text_tertiary.dark.to_hex(),
            usage: "时间戳/占位",
        },
        TokenRow {
            name: "accent",
            light: t.accent.light.to_hex(),
            dark: t.accent.dark.to_hex(),
            usage: "进度条/Tab 激活",
        },
        TokenRow {
            name: "accent_hover",
            light: t.accent_hover.light.to_hex(),
            dark: t.accent_hover.dark.to_hex(),
            usage: "hover",
        },
        TokenRow {
            name: "accent_soft",
            light: t.accent_soft.light.to_hex(),
            dark: t.accent_soft.dark.to_hex(),
            usage: "激活行底",
        },
        TokenRow {
            name: "accent_border",
            light: t.accent_border.light.to_hex(),
            dark: t.accent_border.dark.to_hex(),
            usage: "彩色描边",
        },
        TokenRow {
            name: "on_accent",
            light: t.on_accent.light.to_hex(),
            dark: t.on_accent.dark.to_hex(),
            usage: "accent 上的字",
        },
        TokenRow {
            name: "highlight",
            light: t.highlight.light.to_hex(),
            dark: t.highlight.dark.to_hex(),
            usage: "品牌柠檬黄",
        },
        TokenRow {
            name: "on_highlight",
            light: t.on_highlight.light.to_hex(),
            dark: t.on_highlight.dark.to_hex(),
            usage: "高亮上的字",
        },
        TokenRow {
            name: "wave_idle",
            light: t.wave_idle.light.to_hex(),
            dark: t.wave_idle.dark.to_hex(),
            usage: "波形未播放",
        },
        TokenRow {
            name: "wave_active",
            light: t.wave_active.light.to_hex(),
            dark: t.wave_active.dark.to_hex(),
            usage: "波形已播放",
        },
        TokenRow {
            name: "success",
            light: t.success.light.to_hex(),
            dark: t.success.dark.to_hex(),
            usage: "状态点",
        },
        TokenRow {
            name: "danger",
            light: t.danger.light.to_hex(),
            dark: t.danger.dark.to_hex(),
            usage: "错误/红点",
        },
        TokenRow {
            name: "hover_fill",
            light: t.hover_fill.light.to_hex(),
            dark: t.hover_fill.dark.to_hex(),
            usage: "hover 面",
        },
        TokenRow {
            name: "pressed",
            light: t.pressed.light.to_hex(),
            dark: t.pressed.dark.to_hex(),
            usage: "按下态",
        },
        TokenRow {
            name: "focus_ring",
            light: t.focus_ring.light.to_hex(),
            dark: t.focus_ring.dark.to_hex(),
            usage: "焦点环",
        },
        TokenRow {
            name: "selection_fill",
            light: t.selection_fill.light.to_hex(),
            dark: t.selection_fill.dark.to_hex(),
            usage: "选中文字底",
        },
        TokenRow {
            name: "disabled_text",
            light: t.disabled_text.light.to_hex(),
            dark: t.disabled_text.dark.to_hex(),
            usage: "禁用文字",
        },
        TokenRow {
            name: "disabled_line",
            light: t.disabled_line.light.to_hex(),
            dark: t.disabled_line.dark.to_hex(),
            usage: "禁用分隔",
        },
        TokenRow {
            name: "shadow",
            light: t.shadow.light.to_hex(),
            dark: t.shadow.dark.to_hex(),
            usage: "硬阴影",
        },
    ];
    rows
}

/// 主题矩阵：12 锚点 × 2 模式的 `accent` / `bg` / `surface` / `text` 十六进制。
///
/// 每行 `anchor_name, mode, accent, bg, surface, text`，供快照测试与设计核对。
pub fn theme_matrix() -> String {
    let mut out = String::from("anchor\tmode\taccent\tbg\tsurface\ttext\n");
    for (name, _, c_max) in ANCHORS {
        let t = build_tokens(Accent {
            anchor: anchor_index(name),
            c_max,
        });
        for (mode, light) in [("light", true), ("dark", false)] {
            let r = t.resolve(light);
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                name,
                mode,
                r.accent.to_hex(),
                r.bg.to_hex(),
                r.surface.to_hex(),
                r.text.to_hex(),
            ));
        }
    }
    out
}

fn anchor_index(name: &str) -> usize {
    ANCHORS.iter().position(|(n, _, _)| *n == name).unwrap_or(0)
}

/// 默认主题的 token 表（TSV，供 `TOKENS.md` 与快照测试）。
pub fn token_tsv() -> String {
    let mut out = String::from("name\tlight\tdark\tusage\n");
    for row in token_table() {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            row.name, row.light, row.dark, row.usage
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打印 token 表与主题矩阵（供人工生成 `docs/TOKENS.md`）。
    ///
    /// ```text
    /// cargo test -p reimuplayer --lib dump_tokens_for_docs -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "输出文档用 token 表"]
    fn dump_tokens_for_docs() {
        println!("===== TOKENS (TSV) =====");
        println!("{}", token_tsv());
        println!("===== THEME MATRIX =====");
        println!("{}", theme_matrix());
    }

    #[test]
    fn token_table_covers_all_expected_names() {
        let names: Vec<&str> = token_table().iter().map(|r| r.name).collect();
        for expected in [
            "bg",
            "surface",
            "surface_raised",
            "surface_sunken",
            "line_strong",
            "line",
            "line_subtle",
            "text",
            "text_secondary",
            "text_tertiary",
            "accent",
            "accent_hover",
            "accent_soft",
            "accent_border",
            "on_accent",
            "highlight",
            "on_highlight",
            "wave_idle",
            "wave_active",
            "success",
            "danger",
            "hover_fill",
            "pressed",
            "focus_ring",
            "selection_fill",
            "disabled_text",
            "disabled_line",
            "shadow",
        ] {
            assert!(names.contains(&expected), "缺少 token {expected}");
        }
    }

    #[test]
    fn theme_matrix_has_twenty_four_combinations() {
        let lines = theme_matrix().lines().count();
        // 表头 1 行 + 12 锚点 × 2 模式
        assert_eq!(lines, 1 + 24);
    }

    #[test]
    fn hex_values_are_well_formed() {
        for row in token_table() {
            for h in [&row.light, &row.dark] {
                assert_eq!(h.len(), 7, "{} 不是 #RRGGBB", h);
                assert!(h.starts_with('#'));
                assert!(h[1..].chars().all(|c| c.is_ascii_hexdigit()));
            }
        }
    }
}
