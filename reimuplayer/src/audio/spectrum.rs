//! 实时频谱分析：环形采样缓冲 + 手写 radix-2 FFT + 对数频带折叠。
//!
//! 为什么手写而不是引入 `rustfft`（`ENGINEERING_SPEC.md`「复杂度需要理由」）：
//! 本模块只需要**定长、单次、2 的幂**的前向实数变换（2048 点），每帧一次，
//! 开销在微秒级。为此引入一个带 SIMD 抽象层与多后端代码生成的依赖，
//! 换来的性能提升在 60fps 下不可观测，而体积、许可证与供应链风险是实打实的。
//!
//! 线程模型（`DESIGN_SPEC.md` 15.2）：
//! - **音频线程**只做 `push_mono`，用 `try_lock` 抢锁，抢不到就丢样本——
//!   绝不在音频回调里等待锁。
//! - **UI 线程**调 [`SpectrumShared::bands`] 做 FFT 与频带折叠，持有锁约 30µs。
//!
//! 显示语义：输出是 `[0.0, 1.0]` 的 26 段电平，含快起慢落的平滑，
//! 直接对应 `gui::widgets::EQ_BARS` 根柱子。

use std::borrow::Cow;
use std::sync::OnceLock;

/// 单次 FFT 的采样点数。
///
/// 2048 @ 44.1kHz → 频率分辨率 ≈ 21.5Hz，低频段（60Hz 起）不至于糊成一格。
pub const FFT_SIZE: usize = 2048;

/// 频带（EQ 柱）数量。
///
/// **唯一事实源**：`gui::widgets::EQ_BARS` 直接引用本常量，不得各写一份。
pub const BANDS: usize = 26;

/// 一帧频谱。定长数组，无堆分配，可直接按值传递。
pub type Bands = [f32; BANDS];

/// 频带下限（Hz）。
const F_MIN: f32 = 60.0;
/// 频带上限（Hz）。超过奈奎斯特频率时按 [`F_MAX_NYQUIST_RATIO`] 自动收缩。
const F_MAX: f32 = 16_000.0;
/// 上限相对奈奎斯特频率的留白比例（避开抗混叠滤波的滚降区）。
const F_MAX_NYQUIST_RATIO: f32 = 0.92;

/// 显示动态范围下界（dBFS）。
const DB_FLOOR: f32 = -60.0;
/// 显示动态范围上界（dBFS）。
const DB_CEIL: f32 = -6.0;

/// 高频补偿基准频率（Hz）。
const TILT_REF_HZ: f32 = 300.0;
/// 高频补偿斜率（dB / 倍频程）。
///
/// 音乐能量天然集中在低频，线性显示会让右半屏几乎不动；
/// 按 2.5dB/oct 提亮高频是频谱仪的标准做法。
const TILT_DB_PER_OCT: f32 = 2.5;
/// 高频补偿上限（dB）。
const TILT_MAX_DB: f32 = 12.0;

/// 回落平滑系数：上一帧权重。越大越黏。
const RELEASE: f32 = 0.78;

/// 音频线程批量攒够多少单声道样本才抢一次锁。
///
/// 44.1kHz 下单样本抢锁是每秒 4.4 万次；攒 512 个降到 86 次/秒，
/// 且仍保证 ~11ms 的刷新粒度，远快于 60fps 的一帧。
pub const TAP_BATCH: usize = 512;

// ---------------------------------------------------------------------------
// 手写 radix-2 FFT
// ---------------------------------------------------------------------------

/// 旋转因子表缓存（长度 `FFT_SIZE / 2`，`tw[k] = exp(-2πik / FFT_SIZE)`）。
static TWIDDLES: OnceLock<Vec<(f32, f32)>> = OnceLock::new();

/// 构造长度 `n / 2` 的旋转因子表。用 f64 算再降精度，避免 f32 累积误差。
fn build_twiddles(n: usize) -> Vec<(f32, f32)> {
    (0..n / 2)
        .map(|k| {
            let ang = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
            (ang.cos() as f32, ang.sin() as f32)
        })
        .collect()
}

/// 取 `n` 点变换所需的旋转因子表；`n == FFT_SIZE` 时复用全局缓存。
fn twiddles_for(n: usize) -> Cow<'static, [(f32, f32)]> {
    if n == FFT_SIZE {
        Cow::Borrowed(TWIDDLES.get_or_init(|| build_twiddles(FFT_SIZE)).as_slice())
    } else {
        Cow::Owned(build_twiddles(n))
    }
}

/// 原地基 2 时间抽取 FFT（`re`/`im` 长度必须相等且为 2 的幂）。
///
/// 采用位反转置换 + 蝶形迭代，旋转因子查表，复杂度 `O(n log n)`。
pub fn fft_in_place(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    assert_eq!(n, im.len(), "实部虚部长度必须一致");
    assert!(n.is_power_of_two(), "FFT 长度必须是 2 的幂");
    if n <= 1 {
        return;
    }

    // 位反转置换
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let tw = twiddles_for(n);
    let mut len = 2usize;
    while len <= n {
        let half = len / 2;
        let step = n / len;
        let mut base = 0usize;
        while base < n {
            for k in 0..half {
                let (wr, wi) = tw[k * step];
                let i0 = base + k;
                let i1 = i0 + half;
                let (ur, ui) = (re[i0], im[i0]);
                let (vr, vi) = (re[i1], im[i1]);
                let tr = vr * wr - vi * wi;
                let ti = vr * wi + vi * wr;
                re[i0] = ur + tr;
                im[i0] = ui + ti;
                re[i1] = ur - tr;
                im[i1] = ui - ti;
            }
            base += len;
        }
        len <<= 1;
    }
}

// ---------------------------------------------------------------------------
// 共享缓冲
// ---------------------------------------------------------------------------

/// 音频线程写入、UI 线程读取的频谱状态。
///
/// 由 [`crate::audio::rodio_engine::RodioEngine`] 持有一个 `Arc<Mutex<_>>`：
/// 音频线程那份在 `TapSource` 里，UI 线程那份在引擎字段里。
pub struct SpectrumShared {
    /// 环形样本缓冲，长度恒为 [`FFT_SIZE`]。
    ring: Vec<f32>,
    /// 下一个写入位置。
    pos: usize,
    /// 已写入的有效样本数（上限 [`FFT_SIZE`]）。
    filled: usize,
    /// 解码器采样率（Hz）。未知时为 0，`bands()` 直接返回 `None`。
    rate: f32,
    /// 上一帧各频带电平，用于回落平滑。
    prev: Bands,
}

impl Default for SpectrumShared {
    fn default() -> Self {
        Self::new()
    }
}

impl SpectrumShared {
    pub fn new() -> Self {
        Self {
            ring: vec![0.0; FFT_SIZE],
            pos: 0,
            filled: 0,
            rate: 0.0,
            prev: [0.0; BANDS],
        }
    }

    /// 设置采样率。换曲时调用，同时清空缓冲（旧曲的样本不属于新曲）。
    pub fn set_rate(&mut self, rate: u32) {
        let rate = rate as f32;
        if (self.rate - rate).abs() > f32::EPSILON {
            self.rate = rate;
            self.reset();
        }
    }

    /// 清空样本与平滑状态（停止/换曲时调用）。
    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
        self.filled = 0;
        self.prev = [0.0; BANDS];
    }

    /// 音频线程入口：推入一个**单声道**样本。
    ///
    /// 本方法不得分配、不得阻塞；调用方负责把多声道混成单声道。
    pub fn push_mono(&mut self, sample: f32) {
        self.ring[self.pos] = sample;
        self.pos = (self.pos + 1) % FFT_SIZE;
        if self.filled < FFT_SIZE {
            self.filled += 1;
        }
    }

    /// 已缓冲的样本数。
    pub fn filled(&self) -> usize {
        self.filled
    }

    /// 计算当前帧的 26 段电平。
    ///
    /// 样本不足 [`FFT_SIZE`] 的一半或采样率未知时返回 `None`——
    /// 刚换曲的那一小段宁可显示静止，也不要拿半窗数据画出误导性的形状。
    pub fn bands(&mut self) -> Option<Bands> {
        if self.rate <= 0.0 || self.filled < FFT_SIZE / 2 {
            return None;
        }

        let mut re = [0.0f32; FFT_SIZE];
        let mut im = [0.0f32; FFT_SIZE];

        // 环形缓冲按时间顺序展开；未填满时数据位于 [0, filled)。
        let start = if self.filled < FFT_SIZE { 0 } else { self.pos };
        for (i, slot) in re.iter_mut().enumerate() {
            let s = self.ring[(start + i) % FFT_SIZE];
            // Hann 窗：抑制频谱泄漏，否则单音会在相邻柱上留下鬼影
            let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos();
            *slot = s * w;
        }

        fft_in_place(&mut re, &mut im);

        // Hann 窗相干增益 0.5，满幅正弦的峰值幅度为 N/4；乘 4 归一化到 1.0
        let scale = 4.0 / FFT_SIZE as f32;
        let bin_hz = self.rate / FFT_SIZE as f32;
        let nyquist = self.rate * 0.5;
        let f_max = F_MAX.min(nyquist * F_MAX_NYQUIST_RATIO).max(F_MIN * 2.0);
        let ratio = f_max / F_MIN;
        let half_bins = FFT_SIZE / 2;

        let mut out = [0.0f32; BANDS];
        for (i, slot) in out.iter_mut().enumerate() {
            let lo = F_MIN * ratio.powf(i as f32 / BANDS as f32);
            let hi = F_MIN * ratio.powf((i + 1) as f32 / BANDS as f32);
            let k_lo = ((lo / bin_hz) as usize).clamp(1, half_bins - 1);
            let k_hi = (((hi / bin_hz).ceil() as usize).max(k_lo + 1)).min(half_bins);

            // 带内能量取均方根：比取峰值稳，比取均值灵敏
            let mut energy = 0.0f32;
            for k in k_lo..k_hi {
                let m = (re[k] * re[k] + im[k] * im[k]).sqrt() * scale;
                energy += m * m;
            }
            let rms = (energy / (k_hi - k_lo) as f32).sqrt();

            let mut db = 20.0 * (rms + 1e-9).log10();
            // 高频补偿
            let center = (lo * hi).sqrt();
            let tilt = (TILT_DB_PER_OCT * (center / TILT_REF_HZ).log2()).clamp(0.0, TILT_MAX_DB);
            db += tilt;

            let level = ((db - DB_FLOOR) / (DB_CEIL - DB_FLOOR)).clamp(0.0, 1.0);

            // 快起慢落：上升立即跟上，回落按 RELEASE 衰减
            let prev = self.prev[i];
            let smooth = if level >= prev {
                level
            } else {
                prev * RELEASE + level * (1.0 - RELEASE)
            };
            self.prev[i] = smooth;
            *slot = smooth;
        }

        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, bin: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (std::f32::consts::TAU * bin as f32 * i as f32 / n as f32).sin())
            .collect()
    }

    #[test]
    fn fft_peaks_at_input_bin() {
        let n = 64;
        let bin = 7;
        let mut re = sine(n, bin);
        let mut im = vec![0.0f32; n];
        fft_in_place(&mut re, &mut im);

        let mag = |k: usize| (re[k] * re[k] + im[k] * im[k]).sqrt();
        let peak = (0..n)
            .max_by(|a, b| mag(*a).partial_cmp(&mag(*b)).unwrap())
            .unwrap();
        assert!(
            peak == bin || peak == n - bin,
            "峰值应落在输入频点 {}（或镜像 {}），实际 {}",
            bin,
            n - bin,
            peak
        );
        // 其余频点应远小于峰值
        for k in 0..n {
            if k != bin && k != n - bin {
                assert!(mag(k) < mag(peak) * 0.02, "频点 {} 泄漏过大: {}", k, mag(k));
            }
        }
    }

    #[test]
    fn fft_dc_input_lands_in_bin_zero() {
        let n = 32;
        let mut re = vec![1.0f32; n];
        let mut im = vec![0.0f32; n];
        fft_in_place(&mut re, &mut im);
        assert!(
            (re[0] - n as f32).abs() < 1e-3,
            "DC 分量应为 n，实际 {}",
            re[0]
        );
        for k in 1..n {
            assert!(
                re[k].abs() < 1e-3 && im[k].abs() < 1e-3,
                "频点 {} 应为零",
                k
            );
        }
    }

    #[test]
    fn fft_satisfies_parseval() {
        let n = 128;
        let mut re: Vec<f32> = (0..n).map(|i| ((i * 37 % 11) as f32) - 5.0).collect();
        let time_energy: f32 = re.iter().map(|v| v * v).sum();
        let mut im = vec![0.0f32; n];
        fft_in_place(&mut re, &mut im);
        let freq_energy: f32 = re
            .iter()
            .zip(im.iter())
            .map(|(r, i)| r * r + i * i)
            .sum::<f32>()
            / n as f32;
        let diff = (time_energy - freq_energy).abs() / time_energy.max(1.0);
        assert!(diff < 1e-3, "帕塞瓦尔定理不成立，相对误差 {}", diff);
    }

    #[test]
    fn empty_buffer_yields_no_bands() {
        let mut s = SpectrumShared::new();
        assert!(s.bands().is_none(), "无采样率时不得出频谱");

        s.set_rate(44_100);
        for _ in 0..(FFT_SIZE / 2 - 1) {
            s.push_mono(0.0);
        }
        assert!(s.bands().is_none(), "样本不足半窗时不得出频谱");
    }

    #[test]
    fn silence_sits_at_floor() {
        let mut s = SpectrumShared::new();
        s.set_rate(44_100);
        for _ in 0..FFT_SIZE {
            s.push_mono(0.0);
        }
        let b = s.bands().expect("满窗静音应能出频谱");
        for (i, v) in b.iter().enumerate() {
            assert!(*v < 0.02, "静音时第 {} 段应贴地，实际 {}", i, v);
        }
    }

    #[test]
    fn full_scale_tone_drives_its_band_up() {
        let mut s = SpectrumShared::new();
        s.set_rate(44_100);
        // 1kHz 满幅正弦：应点亮中频段，且不越界
        let bin = 1000.0 / (44_100.0 / FFT_SIZE as f32);
        for i in 0..FFT_SIZE {
            let v = (std::f32::consts::TAU * bin * i as f32 / FFT_SIZE as f32).sin();
            s.push_mono(v);
        }
        let b = s.bands().expect("满窗应能出频谱");
        for (i, v) in b.iter().enumerate() {
            assert!((0.0..=1.0).contains(v), "第 {} 段越界: {}", i, v);
        }
        let loudest = b.iter().cloned().fold(0.0f32, f32::max);
        assert!(
            loudest > 0.6,
            "1kHz 正弦应点亮对应频带，实际最高 {}",
            loudest
        );
        // 1kHz 落在对数轴约 0.5 处，峰值不应出现在最低段
        assert!(b[0] < 0.5, "最低段不应被 1kHz 点满，实际 {}", b[0]);
    }

    #[test]
    fn bands_rise_fast_and_fall_slow() {
        let mut s = SpectrumShared::new();
        s.set_rate(44_100);
        let bin = 1000.0 / (44_100.0 / FFT_SIZE as f32);
        for i in 0..FFT_SIZE {
            s.push_mono((std::f32::consts::TAU * bin * i as f32 / FFT_SIZE as f32).sin());
        }
        let loud = s.bands().unwrap();
        let peak_idx = (0..BANDS)
            .max_by(|a, b| loud[*a].partial_cmp(&loud[*b]).unwrap())
            .unwrap();

        // 灌满静音后，同一段应缓慢回落而不是瞬间归零
        for _ in 0..FFT_SIZE {
            s.push_mono(0.0);
        }
        let after = s.bands().unwrap();
        assert!(
            after[peak_idx] > 0.0 && after[peak_idx] < loud[peak_idx],
            "应回落但不归零: {} -> {}",
            loud[peak_idx],
            after[peak_idx]
        );
    }

    #[test]
    fn reset_clears_history() {
        let mut s = SpectrumShared::new();
        s.set_rate(44_100);
        for i in 0..FFT_SIZE {
            s.push_mono((i as f32 * 0.1).sin());
        }
        let _ = s.bands();
        s.reset();
        assert_eq!(s.filled(), 0);
        assert!(s.bands().is_none());
    }

    #[test]
    fn sample_rate_change_clears_buffer() {
        let mut s = SpectrumShared::new();
        s.set_rate(44_100);
        for _ in 0..FFT_SIZE {
            s.push_mono(0.5);
        }
        assert_eq!(s.filled(), FFT_SIZE);
        s.set_rate(48_000);
        assert_eq!(s.filled(), 0, "换采样率必须清空旧样本");
    }

    #[test]
    fn low_sample_rate_shrinks_band_ceiling() {
        let mut s = SpectrumShared::new();
        s.set_rate(22_050); // 奈奎斯特 11kHz < 16kHz
        let bin = 1000.0 / (22_050.0 / FFT_SIZE as f32);
        for i in 0..FFT_SIZE {
            s.push_mono((std::f32::consts::TAU * bin * i as f32 / FFT_SIZE as f32).sin());
        }
        let b = s.bands().expect("低采样率也应能出频谱");
        for v in b {
            assert!(v.is_finite() && (0.0..=1.0).contains(&v), "越界: {}", v);
        }
    }
}
