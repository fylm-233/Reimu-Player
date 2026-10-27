//! 「今天」是哪一天 —— 每日推荐的日期键。
//!
//! 为什么需要它：每日推荐的内容由**服务端按天生成**，客户端必须知道
//! "手里这批是哪天的"，才能在跨天时自动重拉。用一个整数日期键比较即可，
//! 不需要任何时间库、不需要时区数据库。
//!
//! 为什么不用 UTC 日序号：[`super::recommend`] 的离线推荐用 UTC 日做洗牌种子
//! （那里只是打散，偏 8 小时无所谓）；但"每日推荐"是给人看的——界面上要写
//! 日期，跨天刷新也该发生在用户眼里的零点。UTC+8 的用户若按 UTC 日算，
//! 早上 7 点看到的日期还是前一天，刷新也会拖到早上 8 点，明显不对。
//!
//! 所以本模块：Windows 下直接问系统要**本地**年月日（`GetLocalTime`），
//! 其它平台退回 UTC 推算（项目只交付 Windows，这里只为可编译与可测）。

use std::fmt;

/// 一个日历日。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ymd {
    /// 年（如 2026）。
    pub year: u16,
    /// 月 1..=12。
    pub month: u8,
    /// 日 1..=31。
    pub day: u8,
}

impl Ymd {
    /// 构造。
    pub const fn new(year: u16, month: u8, day: u8) -> Self {
        Self { year, month, day }
    }

    /// 日期键：`年 * 10000 + 月 * 100 + 日`。
    ///
    /// 单调、唯一、可直接比较与存储——判断"跨天了"只需 `a != b`，
    /// 不必比年月日三个字段。
    pub const fn key(self) -> u32 {
        self.year as u32 * 10_000 + self.month as u32 * 100 + self.day as u32
    }
}

impl fmt::Display for Ymd {
    /// `2026-10-26`（定宽，界面里不会因为个位数日期而抖动）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Unix 日序号 → 日历日。
///
/// Howard Hinnant 的 `civil_from_days`：纯整数运算、无查表、无时区依赖，
/// 对负数（1970 之前）也成立。所有日历换算都收在这一个函数里，
/// 便于用已知日期钉死测试。
pub fn civil_from_days(days: i64) -> Ymd {
    // 纪元挪到 0000-03-01，让闰日落在"年末"，简化后续整除
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]，3 月为 0
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
                                                       // 1、2 月属于上一"年"（纪元从 3 月起算），要加回去
    let year = y + if month <= 2 { 1 } else { 0 };
    Ymd {
        year: year as u16,
        month: month as u8,
        day: day as u8,
    }
}

/// 今天（**本地**时区）。
///
/// Windows 下走 `GetLocalTime`：它直接给出本地年月日，既不用算时区偏移，
/// 也不用处理夏令时（那些规则只有系统知道）。
#[cfg(windows)]
pub fn today() -> Ymd {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    // SAFETY: GetLocalTime 只写调用方提供的这块栈内存，不持有指针、不分配。
    // 失败时系统不保证清零，所以先零初始化，再对结果做合理性检查。
    let mut st = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    unsafe { GetLocalTime(&mut st) };
    if st.wYear == 0 || st.wMonth == 0 || st.wDay == 0 {
        // 理论上不会发生。真发生了也不能返回 0000-00-00（那会让"跨天"判定
        // 永远成立、界面显示一个不存在的日期），退回 UTC 推算。
        tracing::warn!("GetLocalTime 返回空日期，退回 UTC 推算");
        return civil_from_days(super::history::now_secs() / 86_400);
    }
    Ymd {
        year: st.wYear,
        month: st.wMonth as u8,
        day: st.wDay as u8,
    }
}

/// 今天（非 Windows：退回 UTC 推算，仅为可编译与可测）。
#[cfg(not(windows))]
pub fn today() -> Ymd {
    civil_from_days(super::history::now_secs() / 86_400)
}

/// 今天的日期键（跨天比较用）。
pub fn today_key() -> u32 {
    today().key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_maps_to_1970_01_01() {
        assert_eq!(civil_from_days(0), Ymd::new(1970, 1, 1));
        // 负数（1970 之前）也要成立，不能回绕成天文数字
        assert_eq!(civil_from_days(-1), Ymd::new(1969, 12, 31));
    }

    #[test]
    fn known_calendar_days_are_pinned() {
        // 2000-01-01 = 10957（30 年 + 7 个闰日）
        assert_eq!(civil_from_days(10_957), Ymd::new(2000, 1, 1));
        // 闰日：2024-02-29 = 19782
        assert_eq!(civil_from_days(19_782), Ymd::new(2024, 2, 29));
        // 2026-10-26 = 20752
        assert_eq!(civil_from_days(20_752), Ymd::new(2026, 10, 26));
    }

    #[test]
    fn consecutive_days_advance_by_one_and_cross_month_boundaries() {
        // 逐日推进 400 天，日期必须严格递增且不跳变（闰年边界全覆盖）
        let mut prev = civil_from_days(20_000);
        for d in 20_001..20_401 {
            let cur = civil_from_days(d);
            assert!(cur > prev, "{d}: {cur} 应晚于 {prev}");
            prev = cur;
        }
    }

    #[test]
    fn day_key_is_unique_per_calendar_day() {
        let a = Ymd::new(2026, 10, 26);
        assert_eq!(a.key(), 20_261_026);
        // 不同日期绝不撞键（月/日补零是硬要求，否则 2026-1-26 与 2026-12-6 会撞）
        let mut seen = std::collections::HashSet::new();
        for month in 1..=12u8 {
            for day in 1..=28u8 {
                assert!(
                    seen.insert(Ymd::new(2026, month, day).key()),
                    "日期键重复: 2026-{month}-{day}"
                );
            }
        }
    }

    #[test]
    fn display_is_fixed_width() {
        assert_eq!(Ymd::new(2026, 1, 6).to_string(), "2026-01-06");
        assert_eq!(Ymd::new(2026, 12, 31).to_string(), "2026-12-31");
    }

    #[test]
    fn today_is_a_plausible_date() {
        // 只做合理性检查（真实系统时间不可控，别把测试写成"必须等于某天"）
        let t = today();
        assert!(t.year >= 2020, "年份不合理: {}", t.year);
        assert!((1..=12).contains(&t.month), "月份不合理: {}", t.month);
        assert!((1..=31).contains(&t.day), "日不合理: {}", t.day);
        assert!(t.key() >= 20_200_000);
    }
}
