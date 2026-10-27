NETEASE_INTEGRATION_SPEC.md
===========================

System 6.x 风格音乐播放器：网易云音乐接入规范
============================

> 版本：1.0  
> 项目类型：Rust 原生桌面音乐播放器  
> 目标平台：Windows 10/11 x64  
> UI 风格：Apple Macintosh System 6.x  
> 核心目标：低内存、低体积、稳定播放、在线功能可替换  
> 在线服务候选：NeteaseCloudMusicApi、api-enhanced、ncm-api-rs

* * *

1. 项目目标

-------

在现有 System 6.x 风格音乐播放器中，增加网易云音乐在线服务，提供以下能力：

1. 在线搜索歌曲。

2. 获取歌曲详情和艺术家信息。

3. 浏览歌单与歌单曲目。

4. 获取普通歌词和逐字歌词（如果接口支持）。

5. 获取歌曲播放地址并交给统一音频引擎。

6. 获取登录用户的歌单（作为后续扩展）。

7. 在网络错误、API 不可用或歌曲无法播放时，保持本地音乐功能正常。

8. 不因在线功能引入不必要的浏览器内核、完整 Node.js 运行时或大型服务框架。

### 1.1 非目标

第一阶段不要求实现：

* 在线音乐下载与永久保存。

* 自动绕过会员、版权或地区限制。

* 私人 FM、评论、社交动态。

* 在线 MV 播放。

* 所有网易云音乐接口。

* 完整的网易云音乐官方客户端功能。

不得将第三方 API 的歌曲地址当作永久有效地址，也不得假设所有搜索结果都可以播放。

* * *

2. 技术选型与决策

----------

### 2.1 候选项目

#### A. NeteaseCloudMusicApi

地址：

https://www.npmjs.com/package/NeteaseCloudMusicApi

定位：原始 Node.js API 项目及 npm 发布渠道。

适用场景：

* 已有 Node.js 服务。

* 希望沿用原始接口设计。

* 已经拥有可运行的 Node.js 部署环境。

注意事项：

* 必须检查实际安装版本、依赖和维护情况。

* 不要将 npm 包误认为 Rust 库。

* 使用它通常意味着需要 Node.js 服务或独立的 Node.js 运行环境。

* npm 发布版本与其他仓库分支的实现可能不完全相同。

本项目不应仅因为原始项目知名度高，就直接把 Node.js 纳入 Rust 播放器的必需运行环境。

#### B. api-enhanced

地址：

[GitHub - NeteaseCloudMusicApiEnhanced/api-enhanced: 🔍 网易云音乐 Node.js API Services (Half Refactor &amp; Enhanced) 全网最全的网易云音乐API接口 · GitHub](https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced)

定位：增强版 Node.js API 服务。

项目 README 声明：

* 使用 MIT License。

* 要求 Node.js 22 及以上。

* 推荐使用 pnpm。

* 支持歌曲、搜索、歌单、歌词、用户信息等接口。

* 支持独立服务部署。

参考：  
[GitHub - NeteaseCloudMusicApiEnhanced/api-enhanced: 🔍 网易云音乐 Node.js API Services (Half Refactor &amp; Enhanced) 全网最全的网易云音乐API接口 · GitHub](https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced)

适用场景：

* 需要较广泛的 API 覆盖。

* 希望沿用 Node.js 项目的成熟接口实现。

* 愿意运行独立的 API 服务。

* 需要快速验证在线功能原型。

缺点：

* 增加 Node.js 运行时和依赖。

* 增加服务启动、退出和升级流程。

* 本地服务可能产生额外内存占用。

* 服务端接口的变化需要同步验证。

* 若使用远程自建服务，还需要承担网络与服务器运维成本。

#### C. ncm-api-rs

地址：

[GitHub - SPlayer-Dev/ncm-api-rs: Netease Cloud Music API - Rust native implementation - 网易云音乐 API Rust 原生实现 · GitHub](https://github.com/SPlayer-Dev/ncm-api-rs)

这是需要重点评估的第三个候选。

项目说明它是从 api-enhanced 移植而来的 Rust 原生实现，支持直接作为 Rust 库调用，也支持启用 `server` 功能构建独立 HTTP 服务。其 README 列出了搜索、歌曲播放地址、歌单和歌词等接口，并声明无需 Node.js 运行时。

参考：  
[GitHub - SPlayer-Dev/ncm-api-rs: Netease Cloud Music API - Rust native implementation - 网易云音乐 API Rust 原生实现 · GitHub](https://github.com/SPlayer-Dev/ncm-api-rs)

优点：

* 可以直接从 Rust 调用 API。

* 不需要额外的 Node.js 运行时。

* 可以复用 Rust 的异步网络栈。

* 可选择只引入 SDK，而不编译 HTTP 服务。

* 项目提供了与 Node.js API 接近的接口形式。

风险：

* 项目规模和维护活跃度需要持续观察。

* 不应把 README 中的接口兼容声明视为对所有接口的完整保证。

* 必须测试当前版本所需的登录、歌词、播放地址和歌单功能。

* WTFPL 许可证虽然较宽松，但正式分发前仍应由项目维护者确认是否符合自身许可证政策。

该项目的 README 将自身标注为 WTFPL，并声称约 5 MB 的内存占用；这属于项目方的描述，不应未经实测就作为本播放器的性能保证。

* * *

3. 最终架构建议

---------

### 3.1 优先方案

推荐使用：

**Rust 桌面播放器 + Rust API 适配层 + 可替换 API Provider。**

优先评估 `ncm-api-rs` 作为直接依赖。如果必要接口不完整、维护状态不符合项目要求，使用独立部署的 `api-enhanced` 作为后备方案。

架构如下：
    Classic Mac UI      |      vOnline Music Controller      |      vMusic Provider Trait      |      +------------------------+      |                        |      v                        vNCM Rust SDK             HTTP API Client      |                        |      v                        vNeteaseCloudMusic        Local or RemoteAPI                      api-enhanced      |      vNormalized Track Model      |      vPlayback Controller      |      vAudio Engine

这里的 Provider 是项目内部抽象，不是要求同时启用两个 API 实现。

### 3.2 必须遵守的架构规则

1. UI 不得直接调用第三方 API。

2. 播放器不得依赖第三方 API 的原始 JSON 结构。

3. 所有网络错误必须转换为项目内部统一错误类型。

4. 播放器核心不得依赖 Node.js。

5. API 适配层不得负责音频输出。

6. 在线歌曲与本地歌曲必须统一进入播放控制器。

7. API 失效时，本地播放功能不得受影响。

8. 更换 API Provider 不得要求重写 UI。

9. 远程 API 地址必须允许配置。

10. 不得在代码中硬编码账号密码或 Cookie。

* * *

4. 两种部署方式比较

-----------

### 4.1 方案 A：本机直接调用 Rust SDK

部署结构：
    Player.exe  |  +-- UI  +-- Playback Engine  +-- Online Music Provider        |        +-- Rust API SDK        +-- HTTP Client        +-- TLS

工作流程：

1. 播放器启动。

2. 用户打开在线搜索。

3. Rust API Provider 发起网络请求。

4. 解析响应。

5. 转换成统一歌曲模型。

6. 用户选择歌曲。

7. 获取可播放地址。

8. 将地址交给播放引擎。

优点：

* 不需要 Node.js。

* 不需要启动额外的 HTTP 服务。

* 不需要额外监听本机端口。

* 生命周期与主程序一致。

* 部署结构相对简单。

* 不会产生独立服务忘记退出的问题。

缺点：

* 第三方 API 实现直接进入播放器的依赖树。

* SDK 更新可能要求重新构建播放器。

* 如果 SDK 的加密逻辑或请求实现出现问题，需要自行升级或切换 Provider。

* 不能仅凭 Rust 实现就假设网络请求始终可用。

**推荐作为默认发布架构。**

### 4.2 方案 B：独立 API 服务

可以选择两种服务来源：

* 本机运行 `api-enhanced`。

* 在远程服务器部署 `api-enhanced`，由播放器通过 HTTPS 访问。

本机部署结构：
    Player.exe   |   | HTTP 127.0.0.1   vAPI Service   |   vNeteaseCloudMusic API

远程部署结构：
    Player.exe   |   | HTTPS   vUser-managed API Server   |   vNeteaseCloudMusic API

优点：

* API 实现与播放器分离。

* 可以独立升级 API 服务。

* 多个客户端可以复用服务。

* Node.js 项目原有的接口能力更容易保留。

* 如果服务部署在远程服务器，客户端不必携带完整 API 服务运行时。

缺点：

* 本机部署需要额外服务运行环境。

* 需要处理启动、退出、端口占用和健康检查。

* 远程部署需要服务器、网络和安全维护。

* 用户登录信息的保存与传输需要额外设计。

* 本机 HTTP 服务必须限制为本地访问，不能无意暴露到局域网或公网。

* 远程服务需要 HTTPS、访问控制和合理的日志策略。

### 4.3 方案对照

| 维度          | 本机 Rust SDK | 本机 Node.js 服务 | 远程自建服务    |
| ----------- | ----------- | ------------- | --------- |
| Node.js 运行时 | 不需要         | 需要            | 客户端不需要    |
| 客户端安装复杂度    | 低           | 中             | 低         |
| 服务启动管理      | 无           | 需要            | 无本地服务     |
| 客户端内存       | 通常较低，但需实测   | 额外服务内存        | 通常较低      |
| 网络请求        | 直接调用        | 本机 HTTP 转发    | HTTPS 转发  |
| API 独立升级    | 需要更新客户端 SDK | 可以独立更新        | 可以独立更新    |
| 服务运维成本      | 低           | 低至中           | 中至高       |
| 隐私控制        | 请求由客户端发出    | 本机服务处理        | 需要信任自建服务器 |
| 故障隔离        | 需做好模块隔离     | 较好            | 较好        |
| 推荐用途        | 默认方案        | 兼容后备          | 可选高级方案    |

注意：远程自建服务可以降低客户端安装体积，但不会消除服务端资源占用；它只是把运行成本转移到服务器。

### 4.4 推荐结论

第一阶段使用 Rust SDK 直接调用。

第二阶段根据真实接口测试结果决定是否引入独立服务。

只有在 Rust SDK 缺少必要功能、稳定性不达标或需要独立部署 API 时，才将 `api-enhanced` 作为服务端方案。

不得仅为追求模块化就默认要求每位用户安装 Node.js。

* * *

5. Rust 依赖管理

------------

### 5.1 核心依赖

建议按职责选择依赖，而不是一次性引入完整生态。

| 依赖           | 职责        | 规则                          |
| ------------ | --------- | --------------------------- |
| `ncm-api-rs` | 网易云音乐 API | 先评估必要接口和许可证                 |
| `reqwest`    | HTTP 请求   | 仅在需要自定义 HTTP Provider 时直接引入 |
| `tokio`      | 异步运行时     | 只启用需要的功能                    |
| `serde`      | 序列化与反序列化  | 用于统一数据模型                    |
| `serde_json` | JSON 解析   | 仅处理必要响应                     |
| `url`        | URL 解析    | 避免手工拼接复杂 URL                |
| `thiserror`  | 错误类型      | 可选                          |
| `tracing`    | 日志与诊断     | 可选，避免高频日志                   |
| `rodio`      | 音频播放抽象    | 仅在现有音频引擎不满足需求时考虑            |
| `symphonia`  | 音频解码      | 按实际需要启用格式特性                 |

参考：

* `reqwest`：[reqwest - Rust](https://docs.rs/reqwest/latest/reqwest/)

* `tokio`：[tokio - Rust](https://docs.rs/tokio/latest/tokio/)

* `symphonia`：[symphonia - Rust](https://docs.rs/symphonia/latest/symphonia/)

* `rodio`：[rodio - Rust](https://docs.rs/rodio/latest/rodio/)

`reqwest` 支持异步 HTTP、JSON、TLS 和连接复用；`tokio` 支持异步 I/O。应避免同时引入多个功能重复的 HTTP 客户端。

### 5.2 Cargo 依赖原则

1. 固定经过测试的依赖版本。

2. 提交 `Cargo.lock`。

3. 不要无条件启用 `full`、`all` 或所有编解码器功能。

4. 优先使用已有的异步运行时。

5. 不要为简单 JSON 解析额外引入大型框架。

6. 不要在 UI 线程执行阻塞网络请求。

7. 每次新增依赖都记录用途、许可证和体积影响。

### 5.3 依赖功能控制

以 `reqwest` 为例：

* 仅启用实际需要的 HTTP、JSON、TLS 功能。

* 使用可复用的 HTTP Client。

* 设置合理的连接超时和请求超时。

* 避免每次请求都重新建立独立的客户端实例。

* 对重试进行限制，防止网络异常时产生请求风暴。

对 Tokio：

* 根据现有 UI 框架决定异步运行时的所有权。

* 如果框架本身已有运行时，不要无理由再启动一套。

* 不需要时避免开启不相关的进程、文件系统和网络特性。

* * *

6. 统一 API Provider 接口

---------------------

### 6.1 内部接口

建议建立统一的 Provider Trait。

示例仅用于定义架构，不代表特定第三方 SDK 的真实方法签名：
    use async_trait::async_trait;#[async_trait]pub trait MusicProvider: Send + Sync {    async fn search_songs(        &self,        keyword: &str,        offset: u32,        limit: u32,    ) -> Result<SearchPage, MusicError>;    async fn get_playlist(        &self,        playlist_id: &str,    ) -> Result<Playlist, MusicError>;    async fn get_lyrics(        &self,        song_id: &str,    ) -> Result<Lyrics, MusicError>;    async fn get_playback_source(        &self,        song_id: &str,        quality: AudioQuality,    ) -> Result<PlaybackSource, MusicError>;}

注意：

* 若选用的 Rust SDK 不能提供这些精确方法，需要在适配层实现映射。

* 如果项目不需要运行时动态切换 Provider，可以使用泛型或枚举，避免不必要的动态分发。

* 具体错误类型、数据结构和异步边界由实际技术栈确定。

* 不得将第三方 JSON 对象直接传给 UI。

### 6.2 统一歌曲模型

    pub struct Track {    pub provider: ProviderId,    pub id: String,    pub title: String,    pub artists: Vec<String>,    pub album: Option<String>,    pub duration_ms: Option<u64>,    pub cover_url: Option<String>,}

设计要求：

* 歌曲 ID 必须与 Provider 绑定。

* 本地路径和在线歌曲 ID 不得共用同一个语义字段。

* 专辑封面允许为空。

* 歌曲时长允许未知。

* 艺术家允许多名。

* 不要把播放 URL 当作永久歌曲标识。

* 不要将登录 Cookie 写入歌曲模型。

### 6.3 播放地址模型

    pub struct PlaybackSource {    pub url: String,    pub expires_at: Option<i64>,    pub quality: Option<AudioQuality>,    pub content_type: Option<String>,}

注意：

* `expires_at` 只有在接口确实提供有效期时才填写。

* 未知的过期时间不得伪造。

* 播放 URL 应仅用于播放，不应默认永久缓存。

* 不能将播放 URL 作为歌曲 ID。

* 获取到 URL 后仍需检查网络响应和音频解码是否成功。

* * *

7. 在线功能接口规范

-----------

本节列出需要验证的常见接口路径。实际请求方法、参数、响应结构及鉴权要求，应以选定项目的当前文档和源代码为准。

### 7.1 搜索

常见接口：

`GET /cloudsearch?keywords=关键词`

用途：

* 歌曲搜索。

* 艺术家搜索。

* 专辑搜索。

* 歌单搜索。

实施规则：

1. 使用防抖处理，避免每输入一个字符就立即发起请求。

2. 搜索请求必须支持取消或忽略过期结果。

3. 必须处理空关键词。

4. 必须处理网络错误和空结果。

5. 列表采用分页或分批加载。

6. 搜索结果仅转换为内部 `Track` 模型。

7. 不要在搜索结果阶段批量请求每首歌曲的播放地址。

建议防抖时间：200–350 ms。此数值属于本项目参数，应通过实际体验调整。

### 7.2 歌曲详情

常见接口：

`GET /song/detail?ids=歌曲ID`

用途：

* 获取标题。

* 获取艺术家。

* 获取专辑。

* 获取时长。

* 获取封面地址。

规则：

* 只请求需要的字段。

* 对已有详情使用短期缓存。

* 缓存不得覆盖用户最新的播放状态。

* 不要在每次 UI 重绘时重新请求详情。

### 7.3 歌单

常见接口：

`GET /playlist/detail?id=歌单ID`

用途：

* 获取歌单元信息。

* 获取歌单曲目。

* 显示歌曲列表。

规则：

* 大型歌单必须采用分页或分批获取策略。

* 不要一次性加载并解码所有封面。

* 歌单元信息与歌曲列表分开缓存。

* 处理曲目被删除、无权访问和部分数据缺失。

* 不得假设所有歌单都能完整返回全部曲目。

### 7.4 歌词

常见接口：

`GET /lyric?id=歌曲ID`

若项目支持逐字歌词，可进一步验证对应接口，例如增强项目提供的 `lyric_new`。

规则：

* 普通歌词和逐字歌词必须区分。

* 不支持逐字歌词时回退到普通歌词。

* 歌词缺失不得影响播放。

* 歌词解析必须处理时间戳缺失和格式错误。

* 不得将歌词请求放在音频回调线程。

* 歌词同步应使用实际播放时钟，而不是 UI 动画时钟。

### 7.5 播放地址

常见接口包括：

* `/song/url`

* `/song/url/v1`

* `/check/music`

应根据目标项目的当前接口文档选择合适的方法。

规则：

1. 用户明确播放歌曲时才请求播放地址。

2. 优先选择明确支持的音质等级。

3. 对无权限或不可播放的歌曲显示清晰状态。

4. URL 获取失败时允许用户重试。

5. 不要无限自动重试。

6. URL 失效后可以重新请求，但必须限制次数。

7. 不能将“搜索成功”误判为“可播放”。

8. 不得把会员、地区或版权限制作为必须绕过的错误。

9. 不得默认下载或永久存储远程音频。

### 7.6 登录和个人歌单

登录功能应作为第二阶段。

如果需要登录：

* 优先评估项目支持的合法认证方式。

* 不要在日志中记录密码或完整 Cookie。

* 不要把敏感凭据写入普通配置文件。

* 不要将 Cookie 放入 URL 查询字符串。

* 不要在未经授权的情况下将用户凭据上传到第三方服务器。

* 必须提供退出登录与凭据清除机制。

* * *

8. 网络请求与可靠性

-----------

### 8.1 请求超时

建议的初始参数：

| 类型     | 超时目标   |
| ------ | ------ |
| 搜索请求   | 5–8 秒  |
| 歌曲详情   | 5–8 秒  |
| 歌词请求   | 5–8 秒  |
| 播放地址请求 | 5–10 秒 |
| 封面加载   | 3–5 秒  |

这些是初始工程参数，不是服务端保证。

### 8.2 错误分类

建立内部错误类型：
    NetworkUnavailableRequestTimeoutRateLimitedAuthenticationRequiredAuthenticationExpiredContentUnavailablePlaybackUrlUnavailableUnsupportedResponseInvalidDataProviderUnavailableCancelledUnknown

UI 应根据错误类型显示简短、可理解的消息，而不是直接显示完整 JSON 或内部堆栈。

### 8.3 重试策略

* 超时：允许有限次数重试。

* 限流：优先遵守服务端返回的重试建议。

* 登录失效：要求重新认证。

* 无权限或版权限制：不得无限重试。

* 无效参数：不得重试。

* 用户主动取消：不再重试。

### 8.4 网络状态

在线服务状态必须与本地播放状态分离。

例如：
    Local Playback: PlayingOnline Provider: Unavailable

不能因为在线服务断开就停止本地音频。

* * *

9. 缓存与内存管理

----------

### 9.1 缓存类别

| 数据     | 建议策略      |
| ------ | --------- |
| 搜索结果   | 短期内存缓存    |
| 歌曲详情   | 有上限的内存缓存  |
| 歌单列表   | 按需缓存      |
| 歌词     | 可选磁盘缓存    |
| 播放 URL | 默认不持久化    |
| 封面图片   | 小型 LRU 缓存 |
| 用户凭据   | 专用安全存储    |

### 9.2 缓存约束

1. 所有缓存必须设置上限。

2. 缓存失效时间必须可以调整。

3. URL 与歌曲元数据使用不同的缓存策略。

4. 不得在内存中永久保留所有历史搜索结果。

5. 大型封面必须限制解码尺寸。

6. 播放列表只存储必要元数据。

7. 网络响应不得无限累积。

8. 用户退出登录时清除相应敏感缓存。

### 9.3 封面图片

经典 System 6.x 界面不应依赖大尺寸彩色封面。

推荐：

* 默认不自动加载所有封面。

* 仅在需要时加载当前歌曲封面。

* 可配置是否显示封面。

* 使用适当的缩略图尺寸。

* 缓存容量应有限。

* 失败时显示单色占位图。

这既有利于轻量化，也能保持复古界面的一致性。

* * *

10. 本机服务安全规范

------------

如果选择本机运行 `api-enhanced`：

1. 监听地址必须限定为 `127.0.0.1` 或等价的回环地址。

2. 不得默认监听所有网卡。

3. 必须处理端口被占用的情况。

4. 必须验证服务进程属于当前应用管理的实例。

5. 不得终止其他同名但不属于本应用的服务进程。

6. 不得将未经认证的 API 服务暴露到局域网。

7. 服务退出时应清理由本应用启动的进程。

8. 服务启动失败时，应提示错误并允许继续使用本地音乐。

9. 必须检查代理环境变量，避免意外将请求发送到错误的代理。

10. 不得通过关闭 TLS 验证解决网络连接问题。

若使用远程服务：

* 必须使用 HTTPS。

* 不得把用户 Cookie 发送给不受信任的第三方服务。

* 应设置合理的访问控制和限流。

* 不得将个人账号凭据写入公共日志。

* 不得将远程服务的可用性视为永久保证。

* * *

11. 许可证与分发审查

------------

### 11.1 源代码许可证

`api-enhanced` 声明使用 MIT License。

`ncm-api-rs` 的仓库标注 WTFPL。

MIT 和 WTFPL 是不同的许可证。项目维护者应在正式发布前审查许可证全文、依赖许可证及分发要求。

### 11.2 必须检查的内容

* Rust SDK 的许可证。

* 所有直接和间接依赖的许可证。

* 音频解码器的许可证。

* 字体文件授权。

* 图标与专辑封面授权。

* 是否包含第三方二进制文件。

* 是否需要附带版权声明或许可证文本。

### 11.3 重要区别

**开源 API 项目的许可证，不等于网易云音乐内容的授权。**

MIT 或 WTFPL 仅涉及对应代码的使用条件，不能据此认定歌曲、歌词、专辑封面、音源地址或第三方服务的使用方式都已获得授权。

不得在产品文档中宣称“开源 API 允许自由下载和分发全部音乐”。

* * *

12. 体积与内存预算

-----------

### 12.1 发布预算

Rust 原生方案应优先避免引入 Node.js 运行时。

建议以以下预算作为开发初始目标：

| 项目             | 目标              |
| -------------- | --------------- |
| 基础播放器可执行文件     | 10–30 MB        |
| 在线 API 功能增加的体积 | 尽量控制在 5–15 MB 内 |
| 在线功能运行时额外内存    | 以实测为准，避免持续增长    |
| 常规在线请求并发       | 默认限制在少量请求       |
| 搜索缓存           | 有明确容量上限         |
| 封面缓存           | 有明确容量上限         |

这些是目标值，不是预先保证的结果。Rust 编译选项、UI 框架、TLS 实现、音频解码器和目标平台都会影响实际体积。

如果最终 UI 框架本身较大，应优先控制依赖重复与无用功能，而不是强行要求整个应用必须小于某个固定 MB 数值。

### 12.2 Release 构建建议

在 `Cargo.toml` 中评估：
    [profile.release]opt-level = "s"lto = truecodegen-units = 1strip = true

说明：

* `opt-level = "s"` 倾向于优化体积，不一定是所有场景下的最快配置。

* `lto = true` 可以帮助减少部分冗余代码，但会增加构建时间。

* `codegen-units = 1` 可能改善优化机会，同时增加编译耗时。

* `strip = true` 适用于正式发布构建，但必须保留独立的调试符号或诊断构建方案。

这些选项应在 Release 构建中验证，不能未经测试就同时应用于所有开发配置。

* * *

13. 推荐目录结构

----------

    src/├── app/│   ├── application.rs│   └── state.rs├── ui/│   ├── main_window.rs│   ├── menu_bar.rs│   ├── player_controls.rs│   └── playlist_view.rs├── music/│   ├── mod.rs│   ├── models.rs│   ├── provider.rs│   ├── playback_controller.rs│   └── errors.rs├── providers/│   ├── mod.rs│   ├── ncm_sdk.rs│   ├── ncm_http.rs│   └── local_files.rs├── audio/│   ├── mod.rs│   └── engine.rs├── cache/│   ├── mod.rs│   └── policies.rs├── config/│   ├── mod.rs│   └── settings.rs└── main.rs

如果项目规模较小，可以合并部分文件，但必须保持职责清晰。

* * *

14. 开发阶段与验收

-----------

### M1：API 可行性验证

目标：

* 搜索歌曲。

* 获取歌曲详情。

* 获取歌单曲目。

* 获取歌词。

* 获取播放地址。

* 验证至少一个实际可播放的音源。

验收要求：

* 每个接口都有真实请求测试。

* 每个接口都有错误处理。

* 记录响应字段与实际差异。

* 不能只凭 README 声称支持某接口就判定通过。

### M2：Rust Provider 实现

目标：

* 实现统一 Provider 接口。

* 转换歌曲与歌单模型。

* 增加超时、取消和错误分类。

* 验证 API 不可用时本地播放正常。

验收要求：

* UI 不直接依赖第三方 JSON。

* 搜索结果不触发大量并发播放地址请求。

* 不将敏感信息写入日志。

### M3：播放链路接入

目标：

* 在线歌曲进入统一播放控制器。

* 支持播放、暂停、切歌和 seek。

* 获取播放地址失败时给出提示。

* 支持有限次数的重新请求。

验收要求：

* 不阻塞 UI 线程。

* 不在音频回调中执行网络请求。

* 在线 URL 过期或无效时能够恢复或清晰报错。

### M4：歌词与歌单

目标：

* 加载歌词。

* 同步歌词时间。

* 浏览歌单。

* 搜索与播放列表交互。

验收要求：

* 歌词缺失不影响播放。

* 长歌单不导致内存无限增长。

* UI 继续遵循 System 6.x 的黑白像素风格。

### M5：轻量化与发布

目标：

* 建立 Release 基线。

* 检查依赖。

* 测量安装体积。

* 测量运行内存。

* 测试网络失败场景。

验收要求：

* 发布包不包含开发文件。

* 在线服务不是启动必需条件。

* 缓存有明确上限。

* 所有新增依赖有许可证记录。

* 性能报告包含真实数据。

* * *

15. Codex 执行提示词

---------------

请根据本规范为 Rust 桌面音乐播放器实现网易云音乐在线功能。

执行顺序：

1. 检查当前仓库、Rust 版本、UI 框架、音频引擎和依赖。

2. 检查 `ncm-api-rs` 当前版本的实际接口、许可证、依赖和维护状态。

3. 对比直接调用 Rust SDK 与独立部署 `api-enhanced` 的实现成本。

4. 先实现最小 API 原型：搜索、歌曲详情、歌单、歌词和播放地址。

5. 为在线服务建立独立 Provider 层。

6. 不允许 UI 直接依赖第三方 API 的原始 JSON。

7. 不允许将 Node.js 设为基础播放器的必需运行时。

8. 将在线音乐接入统一播放控制器。

9. 确保网络请求不阻塞 UI 或音频线程。

10. 实现有限重试、请求超时、取消、缓存和错误提示。

11. 在本地音乐模式下，网络不可用不得影响播放。

12. 在实现过程中记录新增依赖、许可证和体积影响。

13. 完成后进行真实接口测试和 Release 构建。

14. 不得虚构接口可用性、性能数据或音乐授权。

15. 输出变更清单、测试结果、已知限制和下一步建议。

最终交付：

* `NETEASE_INTEGRATION_SPEC.md`

* Rust Provider 实现。

* 单元测试与接口测试。

* 依赖许可证清单。

* Release 构建说明。

* 性能与体积报告。

完成标准：在线搜索、歌单、歌词与播放均通过实际测试；本地播放独立可靠；API 可以替换；新增依赖和运行时保持最少。
