# AGENTS.md —— otools-cadcraft 移植约定

给后续接手这个仓库的 agent（和人）的规则。核心一句话：**`upstream/` 一行都不要改**，
所有移植代码都放在 `shell/`（前端）与 `native/`（宿主 MCP 网关的 headless 适配层），这样上游同步永远干净。

## 铁律

1. **`upstream/` 保持上游原样**。它是 `storytold/cadcraft` 在 `upstream.pin`
   里那个 commit 的浅拉取结果，**不在 git 里**（见 `scripts/fetch-upstream.mjs`）。
   任何「顺手改一下上游」的念头都会在下次同步时丢失。**确需裁剪上游时走
   patch 队列**：改动写进 `shell/patches/<名字>.patch`，由 `fetch-upstream.mjs`
   的 `applyPatches()` 在每次拉取后幂等重放（跳过路径 + 重拉成功路径两处调用，
   缺一处就会在脏树重拉后静默丢补丁，报错表现为「上游没有 xx 符号」的隔层错误）。
2. **`shell/src/upload.rs`、`shell/src/download.rs` 是上游原样副本**，不要就地改；
   要改行为就改 `shell/src/otools.rs` 或 `shell/src/main.rs`。
3. **`shell/` 不引入 native cdylib**。OTools 宿主已经注入了
   `tools_webview_read_file` / `tools_webview_write_file` / `tools_webview_list_fonts` /
   文件对话框，够用。
   **例外（已确立）**：`native/` 是为宿主 MCP 网关暴露 headless 能力的独立适配层，
   它不是前端外壳的替代，也不参与 WASM 构建。
4. **改 `shell/Cargo.toml` 的上游依赖版本时，对齐 `upstream/Cargo.toml` 的
   `[workspace.dependencies]`**（egui/eframe 等是 `0.36` 这一档）。

## 上手前必须知道的三件事

- **rustc 版本**：上游 `rust-version = "1.95"`（egui 0.36 用了 1.95 才稳定的
  `Vec::push_mut`）。本机 `Program Files/Rust` 的 1.94.1 会在 `epaint` 上直接
  `E0658`；用 rustup 装的 stable（实测 1.99.0）：`unset RUSTC_WRAPPER`（它是
  持久环境变量，指向 sccache/旧 rustc）并把
  `~/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin` 提到 PATH 最前。
  `scripts/build-wasm.mjs` 也会检查并提示。
- **git 拉取要走 git CLI**：cargo 的 libgit2 过代理会 502。
  `CARGO_NET_GIT_FETCH_WITH_CLI=true` 可解（上游的 `craft-ui` 是 git 依赖）。
- **trunk 首次运行会下载 wasm-bindgen-cli 与 wasm-opt**（GitHub release），
  国内网络可能要直连。

## 移植层的地图

| 文件 | 作用 |
| --- | --- |
| `shell/index.html` | 唯一的 JS：`window.cadcraftOtools`（available/theme/language/openFiles/saveFile/listSystemFonts/readHostFile/probeRequest） |
| `shell/src/otools.rs` | 桥：`pick_open` / `pick_save` / `deliver` / 主题 / 宿主语言 |
| `shell/src/fontbridge.rs` | 宿主字体桥：取系统字体目录喂上游字体库，按需读字节（见下） |
| `shell/src/main.rs` | 外壳：`set_io`、`set_deliver` 的选择、`services()`、`?cpu/?webgl/?sample`、字体桥接线 |
| `shell/src/upload.rs` | 上游副本：`Inbox`，把读到的字节在下一帧喂给 `CadApp` |
| `shell/src/download.rs` | 上游副本：非 OTools 环境下退回浏览器下载 |
| `shell/patches/*.patch` | 对上游的本地裁剪（discord 移除 / 系统字体桥），fetch-upstream 幂等重放 |
| `native/` | 宿主 MCP 网关的 headless 适配层（独立 workspace，见下节）。产物 `lib/{macOS.dylib,Windows.dll,Linux.so}` 不进 git |

## 上游管理：pin + 按需拉取（2026-10-15 从 subtree 迁移）

上游源码**不在**本仓库里，只有 `upstream.pin` 记录了仓库与精确的 40 位 commit：

- **本地/CI 取源码**：`node scripts/fetch-upstream.mjs`（缺失才拉，已对齐就跳过；`--force` 重拉）。
- **同步上游更新**：改 `upstream.pin` 的 `commit` 为新的完整 SHA，重新拉取，
  再 diff `upstream/apps/cadcraft-web/src/*.rs` 与 `shell/src/*.rs`。
- **为什么改**：subtree 会把约 10MB 的快照永久塞进仓库，而且每次同步只增不减。
  迁移时保留了原来 subtree 锁定的 commit（`e707ab4…`），行为与迁移前一致；
  历史里那些 blob 仍在（清掉需要 force-push，别擅自做）。

## native 适配层（宿主 MCP 网关）

`native/` 是把上游现成的 headless 后端（`cadcraft-mcp`）编成动态库的薄胶水，
让外部 AI 工具经宿主的大 MCP 端点操作本插件。它不是前端外壳，**不参与 WASM 构建**，
也与 `shell/` 互不依赖。

- **独立 workspace**（`native/Cargo.toml` 里有 `[workspace]`）：并进上游 workspace 会让
  构建把 UI 依赖（eframe/wgpu）也拉进来。
- **上游源码不在 git 里**：`native/` 用 path 依赖直接吃 `../upstream/crates/*`，
  所以构建前必须先 `node scripts/fetch-upstream.mjs`。
- **通过公开 `Server` 适配**：上游的 `tools` 模块是私有的。`native/src/mcp.rs` 用
  专用线程持有唯一 `Server`，经 `handle_line` 复用上游的工具表、schema 校验和调用逻辑；
  会话必须跨调用持久（`native/src/mcp.rs` 的 `session_state_persists_across_calls` 用例守着）。
- **I/O hook 必须装**：`open` / `save` 依赖上游 `cmd::file::set_io` 等 hook，装配方式与
  上游 CLI/桌面端一致。
- **两个宿主 method**（宿主 spec §5.4 冻结的契约）：
  `__otools_mcp_tools__` / `__otools_mcp_call__`。
- **产物命名是宿主约定**：`lib/macOS.dylib` / `lib/Windows.dll` / `lib/Linux.so`，
  由 `scripts/pack-plugin.mjs` 打进 `.oplg`；二进制**不进 git**。
- **构建/测试**：`cargo test --release --manifest-path native/Cargo.toml`。
  多平台产物由 `.github/workflows/release-plugin.yml` 的 `build-native` / `merge-native` 两个 job 出。

## 常见任务
- **要加一个新的宿主能力**（例如把 DWG 另存 PDF 走宿主）→ 在 `shell/index.html`
  的 `window.cadcraftOtools` 上加函数，在 `shell/src/otools.rs` 加 `extern "C"` 绑定
  和调用点；**不要**改 `upstream/`。
- **要改 OTools 侧的元信息**（名称/图标/关键词）→ `plugin.json`。

### 宿主字体桥（文档内容字体用系统任意字体）

wasm 读不到本机字体（上游 `ttf.rs` 的扫描在 wasm 恒空），而 CAD 的「文档内容字体」
必须是系统里装的任意字体。链路：`listSystemFonts()`（宿主只解析 name 表）→
`cadcraft_fonts::set_system_catalog` 填上游 `Db.paths`（`Source::Host`）→ 应用解析
未加载字体时文件进 `PENDING` 队列 → shell 每帧 `take_pending_fonts` → `readHostFile`
读字节 → `register_host` 装回库（清 `resolved`/`fallback` 缓存）→ 重绘后再解析命中。
UI 侧 `mtext_editor.rs` 的字体下拉经 `cadcraft_fonts::system_families()` 列出宿主目录。
无头验证：主仓 `_probe/cadcraft-fontbridge-probe.mjs`（假宿主 + CDP 鼠标唤醒，
eframe idle 时 `logic()` 不跑，探针每步前必须 `Input.dispatchMouseEvent`）。

## 不要做的事

- 不要**手改** `upstream/` 工作区就想当然：`fetch-upstream.mjs` 发现脏树会
  `checkout -f` 重拉，没进 patch 队列的改动**静默丢失**，构建报「上游没有 xx
  符号」这种隔层错误。要裁剪上游：改 `upstream/` → `git -C upstream diff >
  shell/patches/X.patch` → `git -C upstream apply --check --reverse` 校验 →
  `git -C upstream checkout -f -- .` 打回 → 构建（补丁由 applyPatches 重放）。
  生成 patch 后不要再对 upstream 做其它编辑（还原会连带回退）。
- 不要把 `dist/` 提交进 git（OTools 仓库本地构建）。
- 不要在本仓库跑 `cargo build` 而不管 target：外壳是 wasm-only 的，
  宿主（x86_64）下 `main()` 只会打印一行提示，别把它当编译报错。
