# otools-cadcraft

**CADCraft**（[storytold/cadcraft](https://github.com/storytold/cadcraft)）的 **OTools 插件版**。

上游是一个「纯 Rust、零 Electron、无 webview」的开源 AutoCAD 式 CAD：`crates/*` 是引擎与文件格式，
`crates/ui-egui` 是 egui 界面，`apps/cadcraft-web` 用 trunk 把同一套 UI 编成 WebAssembly 跑在浏览器里。

OTools 插件本身就是「一个 webview + 一套前端静态资源」，所以移植的关键发现是：
**不用重写任何界面**。把上游的 WASM 版当插件前端，再把「文件从哪来、存到哪去」接到
OTools 宿主上，就得到了一个插件。上游 `crates/*` 一行没改。

## 目录

```
otools-cadcraft/
├── upstream.pin              # 上游锁定（storytold/cadcraft 的完整 SHA，唯一真源）
├── upstream/                 # 按 pin 浅拉的上游代码 + 重放补丁后的工作区（不入库）
├── shell/                    # 我们唯一新写的 Rust 代码：OTools 外壳
│   ├── src/main.rs           #   改编自上游 apps/cadcraft-web/src/main.rs
│   ├── src/otools.rs         #   OTools 宿主桥（对话框 + 读写文件）
│   ├── src/upload.rs         #   上游原样副本（拖放/打开的文件进 Inbox）
│   ├── src/download.rs       #   上游原样副本（非 OTools 环境下的浏览器下载回退）
│   ├── index.html            #   入口 + window.cadcraftOtools 这段 JS 桥
│   ├── Trunk.toml            #   dist 指向仓库根的 dist/
│   ├── patches/              #   对上游的本地裁剪（重拉不丢），见「跟进上游」
│   └── Cargo.toml            #   path 依赖 ../upstream/crates/*，不复制上游代码
├── native/                   # 宿主 MCP 网关的 headless 适配层（独立 workspace）
├── plugin.json               # OTools 插件清单（entry = dist/index.html）
├── scripts/
│   ├── fetch-upstream.mjs    # 按 pin 拉上游源码 + 幂等重放 shell/patches
│   ├── build-wasm.mjs        #   trunk 构建 → dist/
│   └── deploy-to-otools.ps1  #   同步到 otools 仓库 plugins/ 目录
├── lib/                      # 三平台 native 产物（.gitignore，本地/CI 构建）
└── dist/                     # 构建产物（.gitignore，本地构建）
```

## 移植思路

上游的网页版本来就没有文件系统，靠三个钩子把「宿主」这一层换成插件需要的能力——
这些钩子是上游自己设计好的，所以**上游零改动**：

| 上游钩子 | 网页版行为 | OTools 插件版行为 |
| --- | --- | --- |
| `Services::pick_open` | `<input type=file>` 选文件 | `window.otools.dialog.open` → `readHostFile` 读盘 |
| `Services::pick_save` | 返回建议文件名 | 置「弹另存对话框」标记，返回建议文件名 |
| `cmd::file::set_deliver` | 浏览器下载 | `window.otools.writeHostFile` 写盘 |

JS 侧只有 `shell/index.html` 里那一段（约 120 行）：把 `window.otools` 的
`dialog.open/save`、`readHostFile`、`writeHostFile` 包成 `window.cadcraftOtools`，
`shell/src/otools.rs` 用 wasm-bindgen 直接调它。

**`shell/` 不引入 native cdylib**：宿主已经注入了无路径白名单限制的
`tools_webview_read_file` / `tools_webview_write_file` 与文件对话框。宿主 MCP 网关需要的是
另一条能力路径：`native/` 把上游 `cadcraft-mcp` 的 headless 后端编成动态库，经同一套
`__otools_mcp_tools__` / `__otools_mcp_call__` ABI 对外提供 AI 工具。它不参与 WASM 构建。

### 宿主探测：双通道 + 诊断面包屑

宿主的 init script 是一长串拼接（theme → locale → preload.js → env → embedded-runtime.js →
noder runtime.js，共约 130KB），实测个别 webview 里**后半段可能不到岗**——`window.otools`
没定义，但 Tauri 自己的 `__TAURI_INTERNALS__.invoke` 是好的。所以 `shell/index.html`
的桥同时认两条路，任一条通就算「在 OTools 里」：

| 通道 | 入口 | 说明 |
| --- | --- | --- |
| 包装 API | `window.otools` / `window.utools` | 宿主的 `dialog.open/save` + `readHostFile`/`writeHostFile` |
| Tauri IPC 直连 | `window.__TAURI_INTERNALS__.invoke` | 直接调 `tools_webview_pick_files` / `tools_webview_read_file` / `tools_webview_pick_save_path` / `tools_webview_write_file`（含 `window.parent`/`window.top` 兜底查找） |

两条都不通（把 `dist/` 拖进普通浏览器）才退回上游的浏览器下载行为。这个区分是**必须**的：
只认第一条路时，一旦包装没到位，插件会**静默**退化成浏览器下载——表现为「两个保存按钮
每点一次都建一个新文件（`Drawing1 (1).dxf`、`Drawing1 (2).dxf`…）」，正是 0.6.3 修复的事故。

**参数形状**（Rust 侧 `rename_all = "camelCase"`，Tauri 按参数名从 invoke 载荷里取）：
`pick_files`/`pick_save_path` 收 `{ options: {...} }`，`read_file` 收 `{ path }`，
而 `write_file` 收 **`{ request: { path, dataBase64 } }`**——它是结构体参数，平铺传会被
`missing required key \`request\`` 拒掉。宿主自带的 `writeHostFile` 包装恰恰就是平铺传的
（latent bug），所以桥写文件时**只要 raw IPC 在就优先走 raw**，包装只在 raw 不在时兜底
（0.6.4 修复：真机 QSAVE 报 `invalid args \`request\``）。

另存对话框的默认路径会按宿主的 `resolveDialogDirectoryAndName` 规则拆成
`{directory, fileName}`；纯文件名只当建议名（宿主会把裸文件名误判成目录，这是宿主的 quirk）。

排查用：桥会把状态写进 localStorage 的 `cadcraft.otools.diag`（走了哪条通道、每次打开/保存
的落点与模式：`overwrite`/`dialog`）。插件没有 devtools，这是事后唯一能看到页面内部状态
的渠道；WebView2 的 leveldb 里可以直接翻。

## 上游版本

打包所用的上游提交。`upstream.pin` 是唯一真源，这张表用来回溯「哪个插件版本打的是哪份上游代码」，
每次发布都要在这里加一行。

| 插件版本 | 上游标签 | 上游提交 | 打包日期 |
| --- | --- | --- | --- |
| 0.7.0 | v0.8.0 | `714111d528acc86dc6b41e03f9a82cabc1066dca` | 2026-10-21 |
| 0.6.9 | v0.8.0 | `714111d528acc86dc6b41e03f9a82cabc1066dca` | 2026-10-21 |
| 0.6.8 | — | `e707ab4d7cc10102e550ad4c5bcecef1084a9baa` | — |

## 构建

```bash
# 1) 工具链（上游要求 rustc >= 1.95：egui 0.36 用了 1.95 才稳定的 Vec::push_mut）
rustup toolchain install stable
rustup target add wasm32-unknown-unknown
cargo install trunk            # 或从 GitHub releases 下载预编译二进制

# 2) 构建
node scripts/build-wasm.mjs    # 等价于 cd shell && trunk build --release
```

trunk 第一次运行会自己下载 `wasm-bindgen-cli` 与 `wasm-opt`（binaryen），走 GitHub release，
国内网络可能需要能直连的环境。

## 跑起来

```powershell
pwsh -File scripts/deploy-to-otools.ps1     # 同步到 D:\Repos\xyito\otools\otools\plugins\otools-cadcraft
```

然后重载/重启 OTools，应用列表里会出现「CAD 制图」。

插件在纯浏览器里打开也能用（`dist/index.html` 直接拖进浏览器）：此时退回上游的
「下载即保存」行为，只是文件对话框是浏览器自己的。

URL 参数沿用上游：`?webgl` 强制 WebGL2、`?cpu` 用 egui 形状画布（不依赖 WebGPU/WebGL2）、
`?sample` 打开示例图纸、`?theme=dark|light` 覆盖初始主题（默认跟随 OTools 界面主题）。

## 跟进上游

上游代码**不入库随仓库分发**，靠 `upstream.pin`（完整 SHA）+ `scripts/fetch-upstream.mjs`
按需浅拉，再把 `shell/patches/*.patch` 幂等重放到 `upstream/` 上：

```bash
node scripts/fetch-upstream.mjs              # 对齐 pinned commit 后自动重放补丁
node scripts/fetch-upstream.mjs --check      # 只校验是否已对齐，不下载
```

要对齐新上游就改 `upstream.pin` 里的 `commit`（完整 SHA，认「发布标签」而不是 main HEAD），
再跑一次上面的命令；之后要做的事只有两件：

1. `upstream/apps/cadcraft-web/src/{main,upload,download}.rs` 有没有变？变了就把改动
   合并进 `shell/src/` 同名文件（差异就是上面那张表的三处）。
2. `shell/Cargo.toml` 里上游 crate 的版本号（现在写死 `0.36` 等）跟着 `upstream/Cargo.toml`
   的 `[workspace.dependencies]` 走。

补丁冲突的解法：先 `git fetch --depth 1 origin <旧 pin SHA>` 让 3way 有 blob，再在
`upstream/` 里手工合并，最后 `git -C upstream diff HEAD -- <文件>` 重新生成补丁并跑一次
`fetch-upstream.mjs` 验证可重放。注意 `git apply --3way` 会暂存结果，`git diff`（worktree vs
index）会显示为空，重生成补丁必须用 `git diff HEAD`。

当前三个补丁：

| 补丁 | 作用 |
| --- | --- |
| `cadcraft-discord.patch` | 移除 Discord 社区按钮 |
| `cadcraft-sysfonts.patch` | 接入宿主系统字体目录 |
| `cadcraft-zh-commandline.patch` | 补简体中文缺口：11 条命令行提示、`ByBlock/ByLayer/ByColor`、ViewCube 罗盘 `N/S/E/W` |

### 界面语言

上游自带 15 种语言词表（`upstream/crates/ui-egui/src/i18n/*.tsv`，三列：
`context <TAB> source <TAB> translation`），简体中文是 `zh.tsv`，命令行提示、属性值这些
显示点都走 `i18n::t()`，所以补词表即可生效，不用改代码。

上游 `zh.tsv` 漏译的条目由 `cadcraft-zh-commandline.patch` 补齐（`Select an object to change or`
这类命令行提示、`随块/随层/随色`、罗盘 `北/南/东/西`）。补丁只改 `zh.tsv` 一个文件，
不碰其他语言，因此不受上游「各语言行数一致」类约束影响（cadcraft 的
`i18n/tests.rs` 只强制 `uk` 全覆盖，其他语言缺口仅报告不阻断）。
刻意**保留原文**的条目：`WCS ⌄`、`DWG`/`DXF`、`+LOC`/`−LOC`/`ΔLOC`/`+Bin`/`−Bin`
（缩写与格式名按仓库约定不译），以及只含占位符的 `{name}: {distance} < {angle}°` 等模板。
`Ltscale` 在上游词表里但没有任何 `t()` 调用点（MATCHPROP 的 echo 走
`format!("…")` 直出、不过翻译），属于死键，未译。

## 打包本地安装包（.oplg）

```bash
node scripts/pack-plugin.mjs      # 产物在 dist-pack/：otools-cadcraft-<version>.oplg
node scripts/verify-plugin.mjs    # 打包后自检（强烈建议每次都跑）
```

`pack-plugin.mjs` 做三件事：

1. 从 `plugin.json` 生成**生产清单**——删掉 `devUrl` / `quickDev`（正式包不允许带），
   版本号可被第二个参数覆盖（`node scripts/pack-plugin.mjs 0.6.3`）。
2. 打成扁平结构的 ZIP（`plugin.json` / `logo.svg` / `dist/…` / `lib/…`），即 `.oplg`。
3. 产出一份可直接整目录托管的发布目录：`plugin.json`、`logo.svg`、`SHA256SUMS`、`meta.json`。

`verify-plugin.mjs` 不依赖第三方库（自带最小 ZIP 读取器），校验清单合法性、
SHA256SUMS 覆盖与匹配、无 `devUrl`/`quickDev`、ZIP 条目无重复/无反斜杠/无绝对路径/无 `..`、
逐条 CRC 与解压大小、`entry` 与 `logo.svg` 在包内、入口 HTML 含 OTools 宿主桥、包内有 `.wasm`。

**本地安装**：OTools 插件市场 → 安装本地插件 → 选 `.oplg`（`.zip` 也行），
或把 `dist-pack/` 下那份扁平目录整个丢进宿主插件目录。正式包需要同时含
`lib/{macOS.dylib,Windows.dll,Linux.so}`；本地只改前端、未构建 native 时仍可打包联调，
但发布前 `verify-plugin.mjs` 会要求三平台产物齐全。

## 发布插件（独立发布，不走 otools-publish）

本插件是**独立仓库发布**（与 `compositor-tauri` 同模式，不经过 otools-publish 的宿主流水线）：
自己的 release workflow 构建 → GitHub Release 托管 `.oplg` → 提交同一个插件市场 API。

```bash
# 一条命令：dry-run=false 正式发布 + publish_market=true 上架市场
gh workflow run release-plugin.yml --ref main -R ootools/otools-cadcraft \
  -f dry_run=false -f publish_market=true -f auth_mode=pat -f display_version=0.6.4
```

| 环节 | 做法 |
| --- | --- |
| 触发 | `workflow_dispatch`（手动，四个开关见上）或 push tag `v*`（自动） |
| 构建 | `build-native`：四个平台 runner 编译并测试 native，两份 macOS 由 `merge-native` lipo 成 universal；`build`：`node --test scripts/*.test.mjs` → rust stable + wasm32 + trunk v0.21.14（预编译二进制）→ `node scripts/build-wasm.mjs` |
| 打包校验 | `pack-plugin.mjs --out dist-pack --version <v>` → `verify-plugin.mjs dist-pack`（发布前在 publish job 里再校验一次） |
| Release | tag `plugin-otools-cadcraft-v<v>`，资产 `.oplg` + `logo.svg` + `plugin.json` + `SHA256SUMS` |
| 市场 | `publish-market.mjs`（pat/oidc/login/shared 四种认证）POST 生产清单 + `packageUrl` + `logo` 到市场 API |

配套脚本（均移植自 otools-aigv，各带 `node --test` 回归）：

- `scripts/publish-market.mjs`：市场认证与载荷。秘密只在进程内使用，不写 artifact、不进 job outputs；
  `meta` 子命令给 workflow 输出版本/标签/开关，`preflight` 只检查凭证格式不发起请求。
- `scripts/release-policy.mjs`：只读核验发布标签归属——同版本 tag 已指向别的提交时拒绝覆盖。

**凭证**：市场令牌是 `release` **环境级** secret `XYCLOUD_PAT`（不是仓库级 secret），
在 GitHub 仓库 Settings → Environments → release → Secrets 里维护。workflow 的
`environment: release` 让 publish job 才能读到它。

**发新版 checklist**：改 `plugin.json` 的 `version` → 本地 `node scripts/build-wasm.mjs`
+ 三场景回归 → commit/push → 上面的 `gh workflow run` → 核验 run 全绿、
`gh release view plugin-otools-cadcraft-v<v>` 资产齐全、市场返回成功。

## 已知限制

- **「另存为」改名字后，标题栏仍显示旧名**：上游 `Services::pick_save` 是同步的，
  拿不到用户在对话框里选的新路径；保存本身是对的，只是 `st.path` 仍是建议名。
- **首次保存会弹一次对话框**：打开的文件是通过字节流进来的，上游没有「打开时的路径」可带
  （`open_bytes` 不接收路径）。所以第一次保存走「另存为」，之后再保存直接覆盖上次的路径。
- **WASM 内没有真实文件系统**：上游里少数直接调 `std::fs` 的路径（`SCRIPT {path}`、
  `AUDIT {path}`、`xref` 的按路径重载等）在网页版本来就不通，插件里同样不通；
  需要按文件操作的命令请用对应的 `{data: base64}` 形式。
- **缩略图/最近文件**：OTools 的插件列表用的是 `plugin.json` 的 `icon` 与 `screenshots`，
  与上游自己的启动页无关。

## 许可

上游为 MIT OR Apache-2.0。本仓库的移植层（`shell/`、`plugin.json`、`scripts/`）同样以
MIT OR Apache-2.0 分发；`upstream/` 保留上游的 `LICENSE-MIT` / `LICENSE-APACHE` / `NOTICE`。
