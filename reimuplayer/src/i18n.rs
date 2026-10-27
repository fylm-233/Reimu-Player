//! 界面语言（中 / 英）。
//!
//! 设计取舍：**不做资源文件/查表框架**，而是在每个文案点用
//! [`pick`] 就地二选一（`pick(lang, "中文", "English")`）。
//!
//! 为什么不用 key → 资源表：
//! - 本项目的 UI 文案全是**就地书写**的（`panel_head(ui, "Results / 检索结果", …)`），
//!   抽成 key 表要多维护一份索引，还得保证不漏 key、不写错 key；
//! - 就地二选一由**编译器**保证两种语言都写了，漏写一个参数直接编译失败；
//! - 文案带格式参数时（`format!("曲库已载入 {} 首", n)`）查表还得做占位符替换，
//!   就地写两个 `format!` 更直白。
//!
//! 语言状态存 `ReimuApp::lang`，并持久化在 `%APPDATA%\ReimuPlayer\prefs.json`
//! （见 [`crate::gui::prefs`]）。
//!
//! 中文是**默认**且是"原样"的一侧：现有界面文案以中文/中英混排书写，
//! 切到英文时只替换掉含中文的部分（纯英文的 System 6 标签如 `TAPE SHELF`、
//! 磁带壳上的 `TYPE II · C-60` 属于视觉语言，两种语言下都不动）。

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Lang {
    /// 简体中文（默认；保留现有中英混排观感）。
    #[default]
    Zh,
    /// English（把中文部分全部替换为英文）。
    En,
}

impl Lang {
    /// 全部语言（设置面板按此顺序排按钮）。
    pub const ALL: [Lang; 2] = [Lang::Zh, Lang::En];

    /// 面板显示名。用各自的母语书写，切换前也能认出目标语言。
    pub fn label(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "ENGLISH",
        }
    }

    /// 序号（`prefs.json` 与原子量存储用）。
    pub fn index(self) -> u8 {
        match self {
            Lang::Zh => 0,
            Lang::En => 1,
        }
    }

    /// 由序号还原；越界回落到默认（不 panic——配置文件可能被手改）。
    pub fn from_index(index: u8) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or_default()
    }

    /// 由 `prefs.json` 里的字符串还原（`"zh"` / `"en"`，大小写不敏感）。
    pub fn from_code(code: &str) -> Option<Self> {
        match code.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "cn" | "chinese" => Some(Lang::Zh),
            "en" | "en-us" | "english" => Some(Lang::En),
            _ => None,
        }
    }

    /// 写入 `prefs.json` 的代码。
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// 另一种语言（设置面板的单键切换用）。
    pub fn toggled(self) -> Self {
        match self {
            Lang::Zh => Lang::En,
            Lang::En => Lang::Zh,
        }
    }
}

/// 按语言二选一。
///
/// 纯函数、无全局状态：调用方各自持有 `lang`（`ReimuApp::lang` 或函数参数），
/// 因此单测可以并行跑、不会互相污染。
pub fn pick<'a>(lang: Lang, zh: &'a str, en: &'a str) -> &'a str {
    match lang {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_returns_the_right_side() {
        assert_eq!(pick(Lang::Zh, "中文", "English"), "中文");
        assert_eq!(pick(Lang::En, "中文", "English"), "English");
    }

    #[test]
    fn default_is_chinese() {
        // 默认必须是中文：老用户的观感不能被一次升级改掉
        assert_eq!(Lang::default(), Lang::Zh);
    }

    #[test]
    fn index_roundtrips_and_clamps() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_index(lang.index()), lang);
        }
        assert_eq!(Lang::from_index(9), Lang::default());
    }

    #[test]
    fn code_roundtrips_and_tolerates_case() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Lang::from_code("EN"), Some(Lang::En));
        assert_eq!(Lang::from_code(" zh "), Some(Lang::Zh));
        assert_eq!(Lang::from_code("klingon"), None);
    }

    #[test]
    fn toggled_flips() {
        assert_eq!(Lang::Zh.toggled(), Lang::En);
        assert_eq!(Lang::En.toggled(), Lang::Zh);
    }
}
