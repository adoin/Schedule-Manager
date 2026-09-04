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
- 可选开机启动：Windows 使用当前用户的登录计划任务并自动检测、修复程序路径，macOS 使用用户级 LaunchAgent；可选择启动后最小化到托盘、吸附到桌面或打开主程序。
- 桌面日历挂件：支持 Windows Acrylic/macOS Vibrancy 毛玻璃、锁定或解锁拖动、日期与日程详情浏览，并可跳回主程序编辑；位置与锁定状态仅保存在本机 `desktop-widget.json`，不参与云同步。
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

## 自动构建与发布

- 推送 `main` 时，`Build Windows` 使用固定的 Rust 1.98.0 构建；依赖缓存保存在主分支，包含 `ci` 测试配置和 `release` 配置，后续提交可恢复兼容的缓存。
- 测试仍运行 `--all-targets`，使用无 LTO、无调试符号的 `ci` 配置；正式程序使用 Thin LTO 和 16 个代码生成单元。挂件仅编译实际使用的 Glow 渲染器。
- 正式 EXE 只构建一次，打包使用 `scripts/package.ps1 -Installer -SkipBuild`。安装包与记录提交 SHA、版本、构建编号、SHA256 的清单一起保存 30 天。
- 推送与 `Cargo.toml` 版本一致的 tag（如 `1.0.11`）后，`Publish Release` 只下载同一提交成功构建的安装包。若构建尚未结束，成功事件会自动继续发布；若先构建后打 tag，则直接复用已有安装包。两条路径均校验产物身份和校验和，不再启动第二次 Rust 构建。
- 失败、其他提交、其他分支或过期的构建产物不会发布。需要重新生成时，在 Actions 中重跑对应提交的 `Build Windows`；手动触发工作流仅允许 `main`。
- 不再自动删除旧的构建记录，以免连带删除待发布安装包；由 artifact 保留期限控制产物清理。每次构建还保存 Cargo timings 报告 14 天，便于比较真实编译耗时。

本地验证：`cargo test --profile ci --locked --all-targets`、`python -m unittest discover -s scripts/tests -v`。
首次切换配置需要重新建立缓存，提速应以后续缓存命中的运行数据评估。

## 桌面挂件白屏 / 卡住的诊断

Windows 日志目录：`%LOCALAPPDATA%\Emssion\ScheduleManager\data\logs`。
主程序写入 `schedule-manager.日期.log`，挂件写入 `schedule-desktop-widget.日期.log`（含时间、PID、panic 堆栈、窗口事件和后台心跳）。

新版挂件使用独立后台线程监测界面进展：持续 15 秒无进展时记录执行阶段、窗口/父窗口状态，并通过独立进程保存 `logs\dumps\widget-*.dmp` 线程转储。
休眠或明显的调度间隔不直接算作卡顿；自动转储每次运行最多 3 份，目录保留最近 10 份。
收到关闭请求后仍未退出，8 秒后采集诊断再结束挂件进程；转储最多等待 10 秒。主程序的最终同步仍由主程序完成。

如果画面出现白块但程序仍有响应，后台心跳不一定能自动识别。在终止进程前运行：

```powershell
pwsh.exe -NoLogo -NoProfile -File .\scripts\collect-widget-diagnostics.ps1
```

脚本将挂件与主程序线程转储、进程清单、显卡驱动版本、近期应用/系统错误和日志保存到数据目录的 `diagnostics` 子目录，不会结束被采集的应用进程。
可用 `-WidgetExecutable '新版挂件路径.exe'` 指定转储工具；新版工具也能采集尚未更新的已运行程序。
转储可能包含进程内存中的日程内容，仅保存在本机。分析时请保留与 EXE 匹配的构建产物；需要 Rust 符号的诊断版本可用 `cargo build --profile diagnostic --bins` 构建并保留同目录 PDB。

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
