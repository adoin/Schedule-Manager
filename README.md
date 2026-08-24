# Schedule Manager

Rust + Slint 的 Windows/macOS 日程桌面软件。界面采用无描边卡片、阴影与 gap 布局；未登录时使用本地 SQLite，登录后采用服务端先提交、本地缓存随后确认的数据策略。

## 已实现

- 42 格月历，公历与农历同屏展示，包含春节、元宵、端午、七夕、中秋等农历节日。
- 中国法定节假日和调休数据由服务端拉取、校验并缓存；客户端离线使用最近一次缓存。
- 日程创建、编辑、删除、完成状态、地点和备注。
- 六类发生规则：具体日期时间、法定休息日、法定工作日、每年公历某日、每年农历某日、每天某时刻。
- 多个相对提醒：以分钟数组保存，例如 `10, 60, 1440` 表示提前 10 分钟、1 小时、1 天。
- Windows/macOS 系统通知；本地投递去重。
- Windows/macOS 系统托盘：双击恢复窗口，右键可打开、隐藏或在同步后退出；关闭按钮支持最小化、隐藏到托盘、完全退出三种持久化行为。
- 公历、农历和时间使用弹出选择器；分钟类输入限制为数字。
- Windows EXE/任务栏与 macOS App Bundle 使用统一应用 Logo。
- 邮箱 6 位验证码注册、密码登录、系统钥匙串保存会话。
- SQLite 离线缓存、版本冲突检测、删除墓碑；登录状态下 CRUD 先经服务端确认，再更新本地缓存。
- 一致性检查只在本地与云端内容不同的时候要求选择“采用本地”或“采用云端”。

## 桌面端

```powershell
cargo run --bin schedule-manager
```

默认 API：`https://novel.emssion.com/schedule`。开发时可覆盖：

```powershell
$env:SCHEDULE_API_BASE_URL = 'http://127.0.0.1:8010'
cargo run --bin schedule-manager
```

实时监听 Rust/Slint 源码，自动重新构建并重启应用：

```powershell
.\scripts\dev.ps1

# 使用本地 API，或只验证热构建而不启动窗口
.\scripts\dev.ps1 -ApiBaseUrl 'http://127.0.0.1:8010'
.\scripts\dev.ps1 -NoLaunch
```

构建输出保存在 `target/dev-watch/`，按 `Ctrl+C` 会同时关闭 watcher 管理的应用进程。

Windows 发布：

```powershell
# 首次构建需安装 Rust、Visual Studio C++ Build Tools 与 Strawberry Perl
.\scripts\package.ps1
.\scripts\package.ps1 -Installer
```

macOS 在 Mac 上安装 Rust/Xcode Command Line Tools 后运行：

```bash
bash scripts/package-macos.sh
```

产物位于 `dist/Schedule Manager.app`。脚本会写入 bundle identifier 以支持系统通知；发布签名与公证应在 Apple 开发者环境完成。

## 服务端

服务端是独立 Cargo 包，不编译 Slint：

```bash
cargo build --release --manifest-path server/Cargo.toml
```

主要接口均为 POST：

- `/schedule/health`
- `/schedule/auth/request-code`
- `/schedule/auth/register`
- `/schedule/auth/login`
- `/schedule/sync/pull`
- `/schedule/events/upsert`
- `/schedule/events/delete`
- `/schedule/holidays/refresh`

生产环境变量见 `deploy/schedule-api.service`。SMTP 变量兼容现有 Novel Skills 服务的阿里云 DirectMail 配置。

## 数据来源

法定节假日缓存源采用 [NateScarlet/holiday-cn](https://github.com/NateScarlet/holiday-cn)，其 JSON 包含对应国务院通知 URL。第三方数据源不可用时不会清空已有缓存。
