//! OTools 宿主桥：把「文件从哪来、存到哪去」换成 OTools 宿主的对话框与磁盘读写。
//!
//! 上游 `apps/cadcraft-web` 在浏览器里没有文件系统：打开靠 `<input type=file>`，
//! 保存靠浏览器下载（`download.rs`）。OTools 宿主注入的 `window.otools` 提供了
//! 真正的系统文件对话框与 `readHostFile` / `writeHostFile`（底层是宿主的
//! `tools_webview_read_file` / `tools_webview_write_file`，无路径白名单限制），
//! 所以本模块是插件**唯一**需要新写的 Rust 代码：其余全部复用上游 crate。
//!
//! 与上游的对接点只有三个，都在上游设计好的钩子上，上游源码零改动：
//! - `Services::pick_open` —— 触发打开对话框，读到字节后交给 [`crate::upload::Inbox`]
//! - `Services::pick_save` —— 记住「这次要弹另存对话框」，返回建议文件名
//! - `cmd::file::set_deliver` —— 保存时把字节交给宿主写盘
//!
//! JS 侧的实现见 `shell/index.html` 里的 `window.cadcraftOtools`。

use std::cell::{Cell, RefCell};

use cadcraft_engine::cmd::file::{base64_decode, base64_encode};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys;

use crate::upload::Inbox;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = cadcraftOtools, js_name = "available")]
    fn host_available() -> bool;

    #[wasm_bindgen(js_namespace = cadcraftOtools, js_name = "theme")]
    fn host_theme() -> String;

    /// 宿主界面语言（BCP 47，如 `zh-CN`）：OTools 注入到插件 webview 的
    /// `window.__OTOOLS_LOCALE__.locale` / `document.documentElement.lang`。
    #[wasm_bindgen(js_namespace = cadcraftOtools, js_name = "language")]
    fn host_language_raw() -> String;

    /// 弹打开文件对话框并读取选中文件，解析成 `[{name, path, dataBase64}]`。
    #[wasm_bindgen(js_namespace = cadcraftOtools, js_name = "openFiles")]
    fn host_open_files() -> js_sys::Promise;

    /// `{name, dataBase64, forcePick}` → `{path}` 或 `null`（用户取消）。
    #[wasm_bindgen(js_namespace = cadcraftOtools, js_name = "saveFile")]
    fn host_save_file(request: JsValue) -> js_sys::Promise;

    /// 宿主投递的「打开方式」：JS 侧已把文件读成字节攒好，这里一次取走。
    /// 元素形如 `{path, name, dataBase64}`（见 `shell/index.html` 的投递段）。
    #[wasm_bindgen(js_namespace = cadcraftOtools, js_name = "takeDelivered")]
    fn take_delivered_raw() -> js_sys::Array;
}

thread_local! {
    /// 读到/存下的文件交给它，下一帧由 `Shell::logic` 打开（上游的 Inbox 机制）。
    static INBOX: RefCell<Option<Inbox>> = const { RefCell::new(None) };
    /// 下一次 `deliver` 是否**必须**弹另存对话框（「另存为」置真，「保存」用上次路径）。
    static FORCE_PICK: Cell<bool> = const { Cell::new(false) };
}

/// 宿主是否可用（不在 OTools 里时退回上游的浏览器行为：下载文件）。
pub fn available() -> bool {
    host_available()
}

/// OTools 的界面主题，用于给 egui 一个初始主题（`""` = 跟随系统）。
pub fn theme() -> String {
    host_theme()
}

/// 宿主界面语言（BCP 47，如 `zh-CN`）：跟随 OTools 宿主语言切换。
pub fn host_language() -> String {
    host_language_raw()
}

/// 记住 Inbox：打开的字节和报错都通过它送进应用。
pub fn install(inbox: Inbox) {
    INBOX.with(|slot| *slot.borrow_mut() = Some(inbox));
}

fn with_inbox(f: impl FnOnce(&Inbox)) {
    INBOX.with(|slot| {
        if let Some(inbox) = slot.borrow().as_ref() {
            f(inbox);
        }
    });
}

/// `Services::pick_open`：弹宿主的打开对话框，读到字节后交给 Inbox 在下一帧打开。
///
/// 同步返回 `None`（和上游网页版一样：真正的文件是异步送进 Inbox 的）。
pub fn pick_open() -> Option<String> {
    wasm_bindgen_futures::spawn_local(async move {
        match open_files().await {
            Ok(files) => {
                for (name, _path, bytes) in files {
                    with_inbox(|inbox| inbox.receive(name, bytes));
                }
            }
            Err(e) => with_inbox(|inbox| inbox.refuse(e)),
        }
    });
    None
}

/// `Services::pick_save`：置「下次弹另存对话框」并返回建议的文件名。
///
/// 上游网页版的 `pick_save` 也是直接返回这个名字；保存路径最终由 [`deliver`] 决定。
pub fn pick_save(name: &str) -> Option<String> {
    FORCE_PICK.with(|flag| flag.set(true));
    Some(name.to_string())
}

/// 一次打开的结果：`(文件名, 完整路径, 字节)`。
type OpenedFile = (String, String, Vec<u8>);

async fn open_files() -> Result<Vec<OpenedFile>, String> {
    let value = JsFuture::from(host_open_files()).await.map_err(js_error)?;
    let mut out = Vec::new();
    for entry in js_sys::Array::from(&value).iter() {
        let name = field(&entry, "name").unwrap_or_else(|| "Drawing.dxf".to_string());
        let path = field(&entry, "path").unwrap_or_default();
        let data = field(&entry, "dataBase64").unwrap_or_default();
        let bytes = base64_decode(&data).ok_or_else(|| format!("{name}: 宿主返回的内容不是合法的 base64"))?;
        out.push((name, path, bytes));
    }
    Ok(out)
}

/// 取走宿主投递的文件并交给 Inbox：下一帧由 `Shell::logic` 的 `drain` 打开
/// （与「文件 ▸ 打开」和拖放同一条路，含 256MB 上限校验）。
///
/// 方向与 [`deliver`] 相反：宿主在系统里双击已绑定扩展名的图形 → 投递队列 →
/// 本函数取走 → 上游 `CadApp::open_bytes`。
pub fn drain_delivered() {
    let raw = take_delivered_raw();
    for item in raw.iter() {
        if !item.is_object() {
            continue;
        }
        let name = field(&item, "name").unwrap_or_else(|| "Drawing.dxf".to_string());
        let data = field(&item, "dataBase64").unwrap_or_default();
        match base64_decode(&data) {
            Some(bytes) => with_inbox(|inbox| inbox.receive(name, bytes)),
            None => with_inbox(|inbox| inbox.refuse(format!("{name}: 宿主投递的内容不是合法的 base64"))),
        }
    }
}

/// `cmd::file::set_deliver`：把保存出的字节交给宿主写盘。
///
/// 引擎的钩子是同步的 `fn`，而宿主的对话框是 Promise，所以这里发起一次后台任务并
/// 立刻返回 `Ok`（与上游网页版「下载已开始即算已保存」的语义一致）。写盘失败会
/// 通过 Inbox 报在命令行与状态栏上。
pub fn deliver(name: &str, _mime: &str, bytes: &[u8]) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("需要一个文件名".into());
    }
    let force = FORCE_PICK.with(|flag| flag.replace(false));
    let request = save_request(name, &base64_encode(bytes), force);
    let name = name.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        match save_file(&request, &name, force).await {
            Ok(Some(path)) => log::info!("saved {name} -> {path}"),
            Ok(None) => with_inbox(|inbox| inbox.refuse("保存已取消".to_string())),
            Err(e) => with_inbox(|inbox| inbox.refuse(format!("保存失败：{e}"))),
        }
    });
    Ok(())
}

fn save_request(name: &str, data: &str, force_pick: bool) -> JsValue {
    let request = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("name"), &JsValue::from_str(name));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("dataBase64"), &JsValue::from_str(data));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("forcePick"), &JsValue::from_bool(force_pick));
    request.into()
}

/// 返回落盘的真实路径；`None` = 用户在对话框里取消。
async fn save_file(request: &JsValue, name: &str, _force_pick: bool) -> Result<Option<String>, String> {
    let value = JsFuture::from(host_save_file(request.clone())).await.map_err(|e| {
        let message = js_error(e);
        if message.is_empty() { format!("{name}: 宿主保存失败") } else { message }
    })?;
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    Ok(field(&value, "path"))
}

/// 取对象上的字符串字段。
fn field(value: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(value, &JsValue::from_str(key)).ok()?.as_string()
}

fn js_error(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
