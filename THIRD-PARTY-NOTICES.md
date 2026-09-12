# THIRD-PARTY NOTICES

TokenCalendar 以 **MIT** 许可发布（见 [LICENSE](LICENSE)）。本文件列出随本项目分发、
或在本项目源码中被引用的第三方组件及其许可要求。

> 完整依赖树可由构建工具生成核对：Rust 侧 `cargo metadata --format-version 1`，
> 前端侧 `pnpm licenses list`。

## 1. 代码移植（随分发保留上游许可声明）

| 上游项目 | 许可 | 使用范围 |
| --- | --- | --- |
| DSH (DeepSeek Harness) | MIT，Copyright (c) 2026 DeepSeek | `src-tauri/src/collector/dsh.rs` 的多帧 zstd 帧扫描器（`scan_zstd_frames` / `scan_frame_at`）移植自上游 `session-persistence-jsonl/zstd.ts` |

```text
MIT License

Copyright (c) 2026 DeepSeek

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## 2. 图标数据

| 上游项目 | 许可 | 使用范围 |
| --- | --- | --- |
| Lucide (lucide-icons/lucide) | ISC，Copyright (c) 2026 Lucide Icons and Contributors | `src/features/matrix/YearMatrix.tsx` 内联 SVG 图标 `lock-open` / `lock` / `rotate-ccw` 取自 Lucide 官方图标路径数据 |
| Feather（经 Lucide 派生） | MIT，Copyright (c) 2013-present Cole Bemis | 上述 `lock` 图标源自 Feather 项目 |

```text
ISC License

Copyright (c) 2026 Lucide Icons and Contributors

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

```text
The MIT License (MIT) — for the Feather-derived icons

Copyright (c) 2013-present Cole Bemis

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

> 应用图标（`src-tauri/icons/**`）为项目自行提供的素材，不含第三方图标库内容。

## 3. 构建与打包组件

| 组件 | 许可 | 说明 |
| --- | --- | --- |
| Tauri NSIS 模板 | MIT 或 Apache-2.0（Tauri Apps） | `src-tauri/nsis/installer.nsi` 基于 Tauri 官方 NSIS 模板修改，文件内保留了上游出处链接 |
| Tauri / tauri-build / tauri-plugin-dialog / tauri-plugin-single-instance | MIT 或 Apache-2.0 | 桌面框架与插件 |

## 4. 运行时依赖

### Rust（静态链接进可执行文件）

直接依赖均为 MIT / Apache-2.0 系宽松许可（tauri、serde、chrono、rusqlite、calamine、zstd、ureq、windows-sys、window-vibrancy）。
间接依赖中需要说明的部分：

| 许可 | 组件 | 说明 |
| --- | --- | --- |
| MPL-2.0 | `cssparser`、`cssparser-macros`、`dtoa-short`、`option-ext`、`selectors` | 文件级弱著佐权。以**未修改**形式使用，其源码可从上游获取；不影响本项目自有代码的 MIT 授权 |
| Unicode-3.0 | `icu_*`、`zerovec`、`zerofrom`、`yoke`、`litemap`、`writeable`、`tinystr`、`potential_utf`、`zerotrie` | 宽松许可 |
| CDLA-Permissive-2.0 | `webpki-roots` | 宽松许可（证书根列表数据） |
| Zlib / ISC / BSD-3-Clause / CC0-1.0 / 0BSD | 其余若干（`zlib-rs`、`foldhash`、`libloading`、`rustls-webpki`、`untrusted`、`ring`、`dunce` 等） | 均为宽松许可 |

本依赖树中**不存在** GPL / AGPL / SSPL 组件。`r-efi` 声明为
`MIT OR Apache-2.0 OR LGPL-2.1-or-later`（多选一），本项目按 MIT 分支使用；
该 crate 仅在非 Windows 目标的条件依赖中出现。

### 前端（打进 bundle 产物）

`react`、`react-dom`、`scheduler`、`react-colorful`（MIT）、
`@tauri-apps/api`、`@tauri-apps/plugin-dialog`（Apache-2.0 OR MIT）。

构建期工具（`vite` / `rolldown` / `typescript` / `lightningcss` 等）不随产物分发；
其中 `lightningcss` 为 MPL-2.0，同样以未修改形式作为构建工具使用。

## 5. 仅作参考、未复制代码的第三方项目

| 项目 | 许可 | 参考内容 |
| --- | --- | --- |
| junhoyeo/tokscale | MIT | 数据路径、schema 与 token 语义（差分测试基准） |
| jlcodes99/cockpit-tools | **CC BY-NC-SA 4.0** | 仅参考机制思路（端点与绑定流程的公开事实），**未复用任何代码**；本项目与之无许可关联 |
| 前代自有项目 TokenScope | MIT | 前端设计与实现移植（同一作者） |
| openusage 等调研引用 | 未在此登记 | 仅引用公开接口事实，未复制代码 |

> 若后续直接从上述项目复制或改写代码，必须在本文件登记，并在源文件头标注上游项目、
> 版本/提交号与许可。
