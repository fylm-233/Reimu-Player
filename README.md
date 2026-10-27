# Reimu Player

**System 6 × Y2K 磁带机美学桌面音乐播放器** —— Rust + egui 自绘界面，把本地音乐文件与网易云在线曲库统一装进同一盘磁带里。

> **English.** Reimu Player is a desktop music player written in pure Rust (eframe/egui, self-drawn UI).
> It plays local audio files and NetEase Cloud Music (网易云) tracks through one queue, with real-time FFT
> spectrum, halftone cover rendering, synced lyrics, a desktop lyric overlay, three colour themes,
> system tray / global media keys / SMTC integration, and bilingual (zh / en) UI text.
> Quick start: `cargo run --release -p reimuplayer --features gui --bin reimu-player`

---

## 目录

- [项目简介](#项目简介)
- [核心功能](#核心功能)
- [安装与构建](#安装与构建)
- [使用方法](#使用方法)
- [配置说明](#配置说明)
- [项目结构与架构](#项目结构与架构)
- [测试](#测试)
- [已知限制](#已知限制)
- [合规与免责声明](#合规与免责声明)
- [许可证](#许可证)
- [致谢](#致谢)

---

## 项目简介

Reimu Player 是一个**单机桌面播放器**，不是任何音乐服务的官方客户端，也不提供云端存储或多端同步。
它的设计目标是两件同时成立的事：

1. **本地文件开箱即用** —— 拖一个 MP3/FLAC 进窗口就能播，全程不触网；
2. **在线曲库走同一条链路** —— 搜索、歌单、每日推荐、收藏、歌词、封面都来自网易云，
   但在线曲目与本地曲目在队列、播放模式、心动模式里是**同一种东西**，没有两套行为。

界面向 Macintosh System 6 的 1-bit 界面语言与千禧年磁带机（Y2K Tape Deck）致敬：整窗自绘（无系统标题栏）、
1 位硬边描边、封面经有序抖动（ordered dither）转成点阵半调、频谱是黑底酸绿字的仪器面板。
视觉权威稿是仓库根的 [`code (3).html`](code (3).html)，窗口尺寸与间距的对齐规范见
[`src/gui/GUI_ALIGNMENT_PLAN.md`](reimuplayer/src/gui/GUI_ALIGNMENT_PLAN.md)。

当前状态：**开发中（v0.1.0）**，无稳定版、无预编译安装包，接口与配置文件格式可能变动。
主力开发平台为 Windows 10/11；核心库与 CLI 跨平台，GUI 的 Windows 专属能力在非 Windows 上自动降级（见[已知限制](#已知限制)）。

---

## 核心功能

### 播放与曲库

| 能力 | 说明 |
|---|---|
| 本地文件播放 | MP3 / FLAC / WAV / OGG(Vorbis)，单文件装载或整目录扫描；拖放即播 |
| 在线播放 | 网易云取址 → 流式下载落盘 → 装载播放，下载进度分阶段可见，不整段进内存 |
| 统一播放队列 | 本地与在线曲目混排同一队列，支持入队 / 下一首播放 / 跳转 / 上下曲 |
| 四种播放模式 | 顺序 → 随机 → 单曲循环 → 列表循环，左键循环切换，跨启动记忆 |
| 心动模式（智能播放） | 接入 `playmode_intelligence_list`，以当前曲为种子续出个性化歌单；与随机播放互斥 |
| 本地曲目信息匹配 | 经 `search_match`（标题/专辑/艺人/时长/文件 MD5）把本地文件对上网易云歌曲，补齐封面与数字 ID |
| 下一首预缓存 | 当前曲播放期间预取下一首的歌词等资源，切歌不空等 |
| 播放历史 / 收藏 | 本地落盘；收藏与网易云端双向对账同步，不静默覆盖 |
| 自建本地歌单 | 新建 / 改名 / 加曲 / 移曲 / 拖拽排序 / 删除，与云端歌单并列显示 |

### 界面与视觉

| 能力 | 说明 |
|---|---|
| 五页导航 | 播放（Player）/ 搜索（Search）/ 推荐（Recommended）/ 每日推荐（Daily）/ 歌单（Playlists） |
| 磁带机主视觉 | 自绘卡带（含 A/B 面、Dolby NR 丝印、卷带轮随播放转动）、磁带架曲目列表、Spine 书脊视图 |
| 半调封面 | 封面缩到 96 px 后做有序抖动，得到 1-bit 点阵；无封面时按 ID 生成程序化占位图 |
| 实时频谱 | FFT 2048 点仪器面板，60 Hz–16 kHz 分频刻度 |
| 歌词页 | 逐字（YRC）与逐行（LRC）两种时间轴，按实际播放时钟定位，点击行 seek |
| 桌面歌词悬浮窗 | 独立置顶透明窗，支持锁定（点击穿透）、背景不透明度、三种行内进度样式 |
| 三套配色主题 | 东风谷早苗（默认，米白×酸绿×宝蓝）/ 博丽灵梦（暖红×琥珀）/ 雾雨魔理沙（暗色×星金），设置面板一键切换 |
| 全屏模式 | F11 进入、Esc 退出 |
| 右键上下文菜单 | 立即播放 / 下一首播放 / 加入队列 / 加入歌单 / 移出歌单 / 打开或删除自建歌单 / 打开专辑 |
| 中英双语 | 简体中文与 English 全量互切，界面文案集中管理 |

### 系统集成（Windows）

| 能力 | 说明 |
|---|---|
| 全局媒体键 | `RegisterHotKey` 接管播放/暂停、上一首、下一首、停止（独立线程消息循环） |
| SMTC 媒体会话 | 向系统登记会话，锁屏 / 媒体浮层 / 蓝牙耳机显示曲目并回传控制 |
| 系统托盘 | 图标 + 原生右键菜单，可设为「关窗收进托盘」，默认关闭 |
| 登录会话持久化 | Cookie 经 Windows DPAPI（当前用户范围）加密落盘，下次启动自动恢复 |
| 自绘窗口 | 无系统装饰，自绘标题栏 / 边框 / 缩放 / 最小化动画 |

### 在线能力（网易云）

扫码登录 / 手机号+密码 / 粘贴 Cookie 三种方式；关键词搜索（`cloudsearch`）、歌曲批量详情、
歌单分页、账号歌单列表、推荐歌单与排行榜、每日推荐歌曲（需登录）、收藏列表与红心切换、
专辑与歌手曲目、分享链接解析（歌曲 / 歌单 / 专辑）。

音质上限四档可选（128 kbps / 192 kbps / 320 kbps / 无损，默认无损）。
**不做任何 VIP 权限、付费内容或地区版权限制的绕过** —— 不可播就如实报不可播。

---

## 安装与构建

### 环境要求

- Rust 工具链在 **1.95** 上开发验证；未声明 MSRV（`edition = "2021"`）。
- **Windows 10 / 11**：完整功能。
- **Linux / macOS**：核心库、CLI 与 GUI 均可编译运行，但媒体键、SMTC、托盘、桌面歌词、
  DPAPI 会话持久化为 Windows 专属，其余部分自动禁用。
- 运行期不需要 Node.js、Python 或任何外部运行时，产物是单个可执行文件（`tools/` 下的脚本仅供开发）。
- Linux 需具备 OpenGL 驱动（渲染后端为 `glow`）。

### 构建 GUI

GUI 是**可选 cargo feature**，不启用时只编译核心库与 CLI，零额外依赖：

```bash
# 调试构建（保留控制台窗口，日志直接打在终端）
cargo build -p reimuplayer --features gui --bin reimu-player

# 发布构建（opt-level="z" + LTO + strip + panic=abort，无控制台窗口）
cargo build --release -p reimuplayer --features gui --bin reimu-player
```

产物：`target/release/reimu-player.exe`（Windows）。双击即可运行，也可从命令行启动。

### 运行

```bash
# 直接跑
cargo run --release -p reimuplayer --features gui --bin reimu-player

# 启动即扫描本地曲库目录
cargo run --release -p reimuplayer --features gui --bin reimu-player -- --library "D:\Music"
```

### 可执行目标一览

| 目标 | 命令 | 用途 |
|---|---|---|
| `reimu-player` | `cargo run -p reimuplayer --features gui --bin reimu-player` | **产品主程序**（本文档描述的就是它） |
| `reimuplayer` | `cargo run -p reimuplayer --bin reimuplayer -- help` | CLI 验证外壳，每条子命令对应一项核心能力 |
| `gui-test` | `cargo run -p reimuplayer --features gui --bin gui-test` | 在线加载状态 / 超时 / 错误提示的验证窗口 |
| `ncm_probe` | `cargo run -p ncm_probe` | 阶段一网易云 API 预检原型（存档，不参与产品） |

CLI 验证工具示例：

```bash
cargo run -p reimuplayer --bin reimuplayer -- info "D:\Music\song.mp3"   # 元数据与时长
cargo run -p reimuplayer --bin reimuplayer -- play  "D:\Music\song.mp3" --秒数 10
cargo run -p reimuplayer --bin reimuplayer -- search 周杰伦 --limit 5
cargo run -p reimuplayer --bin reimuplayer -- online <歌曲ID> --秒数 15 --lyric
```

---

## 使用方法

### 页面

- **播放（Player）** —— 磁带机主视图、频谱、歌词页、曲目信息，以及设置入口。
- **搜索（Search）** —— 关键词搜索；支持粘贴网易云分享链接（歌曲 / 歌单 / 专辑）。
- **推荐（Recommended）** —— 推荐歌单与排行榜。
- **每日推荐（Daily）** —— 当天推荐歌曲，需登录。
- **歌单（Playlists）** —— 磁带架的多视图容器：云端歌单、本地曲库、我的收藏、
  最近播放、自建本地歌单，以及从专辑 / 艺人页跳来的「专辑曲子 / 歌手曲子」。

### 键盘快捷键

| 键 | 作用 |
|---|---|
| `空格` | 播放 / 暂停 |
| `←` / `→` | 快退 / 快进 5 秒 |
| `Ctrl` + `←` / `→` | 上一首 / 下一首 |
| `↑` / `↓` | 音量 ±5% |
| `M` | 静音切换 |
| `Ctrl` + `F` 或 `/` | 跳到搜索页并聚焦输入框 |
| `F11` | 全屏 |
| `Esc` | 退出全屏 / 关闭菜单、对话框、登录面板 |

此外还有[全局媒体键](#系统集成windows)（播放/暂停、上一首、下一首、停止），窗口未聚焦时同样有效。

### 播放模式与心动模式

控制行的模式键左键按「顺序 → 随机 → 单曲循环 → 列表循环」循环；心形按钮是**心动模式**。
两者互斥：开启心动模式会退出随机，点随机则心动模式自动取消高亮并停止续播。
心动模式需要登录，且当前曲目要有网易云数字 ID —— 本地文件会先走 `search_match` 换取 ID。

### 设置

播放页 → 曲目信息区下方的 `设置` 按钮展开面板，共 7 行：
`账号` / `语言` / `主题` / `音质` / `桌面歌词` / `媒体键` / `托盘`。
所有选择即时写入 `prefs.json`，无需保存按钮。

---

## 配置说明

### 命令行参数（`reimu-player`）

| 参数 | 说明 |
|---|---|
| `--library <dir>` | 启动即扫描该目录作为本地曲库 |
| `--search-timeout-ms <ms>` | 覆盖在线搜索超时（默认见 `ProviderTimeouts::default()`） |

### 数据目录

| 平台 | 路径 |
|---|---|
| Windows | `%APPDATA%\ReimuPlayer` |
| 其他（XDG） | `$XDG_CONFIG_HOME/reimu-player`，回退 `~/.config/reimu-player` |

| 文件 | 内容 | 可否删除 |
|---|---|---|
| `prefs.json` | 用户偏好，见下表 | 删了回默认值 |
| `local_library.json` | 本地曲库索引（可重建缓存） | 可删，下次启动重扫 |
| `local_playlists.json` | 自建本地歌单 | 删了丢歌单 |
| `favorites.json` | 收藏（与云端对账同步） | 删了下次启动从云端拉回 |
| `play_history.json` | 播放历史 | 可删 |
| `session.bin` | DPAPI（当前用户范围）加密的网易云会话 | 删了需重新登录 |
| `covers/` | 封面图缓存 | 可删，会自动重建 |

> 首次启动会把旧品牌目录（`%APPDATA%\Sonora` / `~/.config/sonora`）里**缺失的文件复制**过来：
> 只复制、不移动、不覆盖已有文件，旧目录保持完整，出问题可退回旧版本。
> 旧版 `session.bin` 也仍能解密（内置旧熵值回退，解密成功后自动按新熵值重写一次）。

### `prefs.json` 字段

```json
{
  "version": 1,
  "quality": "LOSSLESS",
  "lang": "zh",
  "tray": false,
  "lyric_opacity": 66,
  "lyric_progress": "line",
  "volume": 72,
  "muted": false,
  "shuffle": false,
  "repeat": 0,
  "theme": 1
}
```

| 字段 | 取值 | 含义 |
|---|---|---|
| `version` | `1` | 格式版本；不符则整份丢弃回默认 |
| `quality` | `STANDARD` / `HIGHER` / `EX-HIGH` / `LOSSLESS` | 音质上限，默认 `LOSSLESS`；比较时忽略大小写 |
| `lang` | `zh` / `en` | 界面语言 |
| `tray` | `true` / `false` | 关窗收进托盘，默认 `false` |
| `lyric_opacity` | `0..=100` | 桌面歌词背景不透明度，默认 `66` |
| `lyric_progress` | `line` / `off` / `acid` | 桌面歌词行内进度条样式，默认 `line`（细白线） |
| `volume` | `0..=100` | 上次音量，启动恢复 |
| `muted` | `true` / `false` | 静音态，与音量分开存 |
| `shuffle` | `true` / `false` | 随机播放 |
| `repeat` | `0` 顺序 / `1` 列表循环 / `2` 单曲循环 | 循环档位 |
| `theme` | `0` 东风谷早苗 / `1` 博丽灵梦 / `2` 雾雨魔理沙 | 主题下标，首次启动默认 `1`（博丽灵梦）；越界夹回最后一套 |

**新增字段一律可选**：老文件缺字段时回落默认，不会连带重置其它设置。
写盘走 `*.tmp` + `rename`，断电不会留下半截 JSON。

### 环境变量

| 变量 | 生效构建 | 说明 |
|---|---|---|
| `RUST_LOG` | 全部 | `tracing` 日志级别过滤，如 `RUST_LOG=reimuplayer=debug` |
| `REIMU_FPS_LOG` | 仅 debug | 周期性打印帧率与耗时统计 |
| `REIMU_PROBE_PAGE` | 仅 debug | 渲染指定页并落一帧 PPM 后退出，用于像素级校对 |
| `REIMU_PROBE_SETTLE` | 仅 debug | 探针等待网络稳定的秒数 |
| `REIMU_PROBE_*` | 仅 debug | 其余探针开关（`_QUERY` / `_SETTINGS` / `_MENU` / `_TOAST` / `_TITLE` / `_CONFIRM` / `_ALBUM` / `_PEEK` / `_DAILY` / `_LANG` / `_LYRIC_WINDOW` / `_LYRIC_SHOT` / `_LYRIC_LOCKED` / `_LYRIC_OPACITY`） |

探针类变量属于开发工具，release 构建中不产生作用。

---

## 项目结构与架构

```
.
├── code (3).html              视觉权威稿（System 6 / Y2K Tape Deck 单窗口设计稿）
├── DESIGN_SPEC.md             视觉与交互规范
├── ENGINEERING_SPEC.md        工程规范（线程模型、状态机、错误分层）
├── NETEASE_INTEGRATION_SPEC.md  网易云接入规范（依赖、超时、重试、许可证记录）
├── docs/                      分阶段计划、审计报告、图标方案、UI 设计稿
├── assets/icon/               应用图标资产（ICO / PNG / 磁贴 / desktop.ini）
├── tools/                     离线辅助脚本（图标打包、UI 审计、截图探针）
├── ncm_probe/                 阶段一 API 预检原型
└── reimuplayer/               播放器本体（库 + 三个二进制）
    └── src/
        ├── lib.rs             模块地图与对外导出
        ├── error.rs           统一错误类型，第三方错误不外泄
        ├── music/             领域模型、媒体装载、播放控制器、Provider 契约
        ├── audio/             音频引擎（rodio 实现 + 确定性 Mock 实现）+ 频谱
        ├── providers/         网易云 Provider 与 HTTP 音频下载器
        ├── i18n.rs            中英双语文案表
        ├── album_theme/       专辑取色纯计算层（OKLab 色彩学，零依赖，当前未接入 UI）
        ├── main.rs            CLI 验证外壳
        ├── bin/               reimu-player（产品）/ gui-test（验证）
        └── gui/               界面层（约 30 个模块：装配、控件、主题、设置、探针等）
```

### 分层规则

```
   UI / CLI
      │
      ▼
 music::PlaybackController   ← 唯一控制入口
      │              │
      ▼              ▼
 audio::AudioEngine   music::MusicProvider
      │                      │
      ▼                      ▼
 rodio / mock         providers::NeteaseProvider
```

硬约束（贯穿整个代码库，有测试兜底）：

- UI **不直接调用**第三方 API，也不依赖第三方原始 JSON；
- 网络错误一律转换为项目内部错误类型；
- 播放器核心不依赖 Node.js 或任何外部运行时；
- 音频回调线程里不做网络、不做文件 IO、不写日志；
- UI 线程零阻塞：所有请求经 `ProviderBridge` 异步派发，用 generation 计数丢弃过期响应；
- 新增依赖必须记录用途、许可证与体积影响。

---

## 测试

```bash
cargo test -p reimuplayer --features gui
```

当前 **651 个测试通过、5 个标记 `#[ignore]`**（需要真实音频设备、真实网络或会占用系统媒体键的
手工用例，`-- --ignored` 单独跑）。测试覆盖的重点：

- 播放状态机与进度计算用 `MockEngine` 的确定性时钟，**无需音频输出设备即可完整测试**；
- 主题层含逐主题 WCAG 对比度断言 —— 新加一套配色若文字不可读会直接失败；
- 设置面板逐行命中区间断言，防止行重叠导致「点 A 触发 B」；
- 配置文件读写、旧目录迁移、DPAPI 熵值回退均有落盘往返测试；
- 界面几何由 `REIMU_PROBE_PAGE` 探针出帧后逐像素校对。

---

## 已知限制

- **仅 Windows 完整支持**：媒体键、SMTC、托盘、桌面歌词、DPAPI 会话在 Linux/macOS 上不可用；
  程序仍可正常播放本地文件与在线曲目。
- **在线播放依赖网易云接口现状**：接入的第三方 SDK `ncm-api-rs` 上游更新缓慢，
  接口变更需要跟随调整（详见 [`docs/NETEASE_API_VERIFICATION.md`](docs/NETEASE_API_VERIFICATION.md)）。
- **无预编译发行版**：目前需自行 `cargo build`。
- **无应用内自动更新**、**无多语言界面之外的本地化**（如日期格式、数字格式）。
- **在线曲目下载落临时目录**播放，非离线缓存管理工具。
- 心动模式、每日推荐、收藏同步需登录；未登录时相关入口给出提示而非静默失败。
- `album_theme`（专辑取色主题）是独立的纯计算层，尚未接入界面渲染。

---

## 合规与免责声明

- 本项目是**独立的第三方客户端**，与网易公司无任何关联，未使用任何非公开的私有协议破解手段。
- 所有在线能力都要求使用者**以自己的网易云账号登录**，账号权限决定能听到什么。
  会员曲目、付费曲目、地区不可播曲目一律**如实上报不可播**，本项目不提供任何绕过。
- 音乐内容的版权归属唱片公司与相应平台。请仅在您拥有合法收听权限的范围内使用本工具，
  并遵守所使用平台的服务条款与当地法律。
- 「网易云音乐」及相关商标、歌曲与封面素材的权利归其权利人所有。
- 登录会话仅以 Windows DPAPI 在**当前用户范围**加密后保存在本机数据目录，
  不上传、不写入日志、不随崩溃报告外发。
- 本项目按「现状」提供，不含任何明示或暗示的保证；因使用本项目造成的任何后果由使用者自行承担。

---

## 许可证

**待发布者最终确认** —— 本仓库当前尚未提交 `LICENSE` 文件，正式开源前需补齐。

拟采用 Rust 生态惯例的 **MIT OR Apache-2.0** 双许可（与本项目自身代码及绝大多数依赖一致），
在仓库根放置 `LICENSE-MIT` 与 `LICENSE-APACHE`，并在 `Cargo.toml` 的 `[workspace.package]`
中声明 `license = "MIT OR Apache-2.0"`。

### 第三方许可证

所有直接依赖的引入理由与特性裁剪记录在
[`NETEASE_INTEGRATION_SPEC.md`](NETEASE_INTEGRATION_SPEC.md) 第 5 节（依赖）与第 11 节（许可证），
逐依赖的实测许可证清单见 [`docs/NETEASE_API_VERIFICATION.md`](docs/NETEASE_API_VERIFICATION.md)。摘要：

| 依赖 | 许可证 | 用途 |
|---|---|---|
| `eframe` / `egui` / `winit` | MIT OR Apache-2.0 | 自绘 GUI 与窗口 |
| `rodio` | MIT OR Apache-2.0 | 音频解码与输出 |
| `lofty` | MIT OR Apache-2.0 | 媒体元数据 |
| `tokio` / `serde_json` / `reqwest` / `thiserror` / `tracing` | MIT OR Apache-2.0 | 异步运行时、序列化、HTTP、错误、日志 |
| `md-5` | MIT OR Apache-2.0 | 本地文件 MD5（`search_match` 入参） |
| `image` / `qrcode` | MIT OR Apache-2.0 | 封面解码、登录二维码 |
| `rfd` | MIT | 本地曲库目录选择对话框 |
| `windows-sys` / `tray-icon` | MIT OR Apache-2.0 | Windows API 绑定、系统托盘 |
| `smtc-player` | MIT | 系统媒体传输控件（SMTC）会话宿主 |
| **`ncm-api-rs`** | **WTFPL** | 网易云 API 客户端 |

`ncm-api-rs` 采用 WTFPL：允许使用、复制、修改、分发，与主流许可证基本都能组合；
但**不含专利授权**，且在部分法域其法律效力存在争议。若贵方合规政策不接受 WTFPL，
请替换该依赖或移除 `providers/ncm.rs` 的在线能力 —— 本地播放不受影响。

### 字体与图标资产

随仓库附带的字体均为 SIL Open Font License 1.1，授权原文与保留声明见
[`reimuplayer/assets/fonts/`](reimuplayer/assets/fonts/)：

| 资产 | 许可证 |
|---|---|
| `Inter_400Regular.ttf` / `Inter_700Bold.ttf` / `Inter_900Black.ttf` | OFL-1.1（`OFL-Inter.txt`） |
| `NotoSansSC_400.ttf` / `NotoSansSC_700.ttf` | OFL-1.1（`OFL-NotoSansSC.txt`） |
| `NotoSansJP_400.ttf` / `NotoSansJP_700.ttf` | OFL-1.1（`OFL-NotoSansJP.txt`） |
| `fa-solid-900.ttf`（Font Awesome Free 图标） | CC-BY 4.0 + OFL-1.1（`LICENSE-FontAwesome.txt`） |
| `assets/icon/` 应用图标 | 本项目原创，随主许可证 |

Font Awesome 的署名要求由 `LICENSE-FontAwesome.txt` 覆盖，再分发时请一并保留该文件。

---

## 致谢

- **`ncm-api-rs`**（SPlayer-Dev）—— 网易云接口封装，其
  [API 文档](https://github.com/SPlayer-Dev/ncm-api-rs/blob/main/docs/API.md) 是心动模式
  （`playmode_intelligence_list`）与本地歌曲匹配（`search_match`）接入的权威依据。
- **`egui` / `eframe`**（emilk 及贡献者）—— 即时模式 GUI，让整窗自绘不必背一套 Web 运行时。
- **Rodio**、**Lofty** —— Rust 音频与元数据生态。
- 「Reimu」与「东风谷早苗 / 博丽灵梦 / 雾雨魔理沙」等主题名取自《东方 Project》，
  系上海爱丽丝幻乐团的既有作品角色，仅作配色命名致敬，与原作无关联。

---

*Reimu Player · v0.1.0 · 用 Rust 写成。*
