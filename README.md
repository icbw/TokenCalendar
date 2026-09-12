# TokenCalendar

> 本地优先的桌面挂件：把本机各类 AI Agent 的 token 用量统一成「Agent/模型 × 日期」的年度热力图，
> 贴桌面常驻、可钻取。Tauri 2 + Rust + React/TS 实现。

## 功能

- **用量矩阵**：Agent/模型 × 日期的年度与月度热力图，支持日/周/累计粒度与双向钻取。
- **本机采集**：只读扫描六类 Agent 的本地日志/数据库（ZCode、WorkBuddy、Claude Code、
  Codex、CodeBuddy、DSH），聚合结果只保存在本机。
- **数据洞察**：用量趋势、模型分布、异常日与积分月报（手写 SVG，不引图表库）。
- **桌面形态**：主窗口 + 贴桌面挂件 + 订阅额度悬浮球；主题（取色/透明度/圆角/毛玻璃）
  与窗口几何本地持久化。

## 界面

- **主界面**：Agent/模型 × 日期的用量矩阵，Token / 积分双口径、日 / 周 / 累计三档粒度，底部趋势曲线与矩阵行联动；

  ![主界面](screenshots/01-main-matrix.png)

- **桌面挂件**：年度热力图贴片，贴桌面常驻、可钻取；

  ![桌面挂件](screenshots/02-widget-heatmap.png)

- **订阅额度悬浮球**：展开为 5 小时 / 7 天额度环（读数、平台切换、刷新），收起为贴边竖条；

  ![悬浮球（展开）](screenshots/03-orb-expanded.png)
  ![悬浮球（收起）](screenshots/04-orb-collapsed.png)

## 快速开始

```bash
pnpm install                  # 前端依赖（pnpm）
pnpm dev:full                 # 开发模式（= tauri dev）
pnpm tauri build --no-bundle  # 生产构建 → src-tauri/target/release/tokencalendar.exe
pnpm tauri build              # NSIS 安装包 → src-tauri/target/release/bundle/nsis/
```

透明/毛玻璃等窗口效果以生产构建为准；dev 模式下打开 DevTools 时 WebView2 会强制不透明。
NSIS 出包会为更新包签名（`src-tauri/tauri.conf.json` 的 `plugins.updater`），需要本地
配置签名私钥；只验证构建可直接用上方 `--no-bundle` 那条。

## 版本

- **0.5.2** —— 悬浮球交互与视觉打磨（展开圆盘 / 贴边竖条 / 悬停浮层 / 刷新动画 /
  多显示器几何重做）、Claude 授权检测修复、新增应用内更新（签名校验 + 自动更新开关）。
- **0.3.7** —— 首个公开版本：主界面 / 桌面挂件 / 订阅额度悬浮球三窗口，
  六源本机采集（ZCode、WorkBuddy、Claude Code、Codex、CodeBuddy、DSH），
  数据洞察（趋势 / 模型分布 / 异常日 / 积分月报），主题化与贴片化工作区吸附。

## 许可

MIT，见 [LICENSE](LICENSE)。第三方组件、署名与许可义务见
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)。
