//! CADCraft 的 OTools 插件外壳。
//!
//! 本文件改编自上游 `apps/cadcraft-web/src/main.rs`（上游 eframe web runner 的外壳）。
//! 改动只有三处，都是「把浏览器能力换成 OTools 宿主能力」：
//!
//! 1. `mod host`（嵌入页面的 postMessage 桥）→ `mod otools`（OTools 宿主桥，我们新写的）
//! 2. `services()` 的 `pick_open` / `pick_save` 走 [`otools`]
//! 3. `set_deliver` 在 OTools 里走 [`otools::deliver`]（写盘），否则退回上游的浏览器下载
//!
//! `upload.rs` / `download.rs` 是上游文件的原样副本。上游更新时，只需把
//! `apps/cadcraft-web/src/{main,upload,download}.rs` 与本目录同名文件对一遍。
//! 上游 `crates/*` 一律不改：它们是 git subtree，保持原样才能干净地跟上上游。

#[cfg(target_arch = "wasm32")]
pub mod download;
#[cfg(target_arch = "wasm32")]
mod fontbridge;
#[cfg(target_arch = "wasm32")]
mod otools;

#[cfg(any(target_arch = "wasm32", test))]
mod panel_layout;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::{Cell, RefCell};

    use cadcraft_engine::Session;
    use cadcraft_ui_egui::{CadApp, Services};
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast as _;

    const CANVAS_ID: &str = "cadcraft_canvas";
    const LOADING_ID: &str = "cadcraft_loading";

    thread_local! {
        /// 宿主语言切换后，下一帧更新 `system_languages` 并重渲染。
        static NEEDS_LANG_SYNC: Cell<bool> = const { Cell::new(false) };
        /// 缓存 egui Context，供语言切换事件触发重绘。
        static APP_CTX: RefCell<Option<egui::Context>> = RefCell::new(None);
    }

    /// 应用、后台读取的文件（File > Open… 与拖放），以及浏览器的
    /// `prefers-reduced-motion`（它是活的，所以「视图 ▸ 辅助功能 ▸ 动画效果 ▸ 使用系统设置」
    /// 能跟随系统设置的变化）。
    struct Shell(CadApp, crate::upload::Inbox, Option<web_sys::MediaQueryList>);

    impl eframe::App for Shell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            // 缓存 Context，并响应宿主语言切换：更新 system_languages 后下一帧重渲染。
            APP_CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
            if NEEDS_LANG_SYNC.with(|f| f.replace(false)) {
                let host = crate::otools::host_language();
                self.0.system_languages = if host.is_empty() {
                    web_sys::window()
                        .map(|w| w.navigator().languages().iter().filter_map(|v| v.as_string()).collect())
                        .unwrap_or_default()
                } else {
                    vec![host]
                };
            }
            // Background reads are consume-based and run on hidden ticks too. Dropped files are
            // input and are read from `ui` only.
            // 宿主投递的「打开方式」：系统里双击已绑定扩展名的图形 → 交给 Inbox，
            // 紧接的 drain 与「文件 ▸ 打开」、拖放走同一条打开路径。
            crate::otools::drain_delivered();
            self.1.drain(&mut self.0);
            self.0.system_animations = self.2.as_ref().map(|reduced| !reduced.matches());
            self.0.logic(ctx);
            // 让插件更纯粹：吞掉所有外部链接（Discord 等社区外链），点击不再跳出插件。
            ctx.output_mut(|o| o.commands.retain(|c| !matches!(c, egui::OutputCommand::OpenUrl(_))));
            // 宿主字体桥：把应用要过的系统字体文件读进来（有界批量，读完请求重绘）。
            crate::fontbridge::poll(ctx);
        }
        /// Forward raw input to the web `CadApp` and capture Tab at the command line.
        fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
            self.0.raw_input_hook(ctx, raw);
            cadcraft_ui_egui::cmdline::capture_tab(ctx, raw);
        }
        /// Preferences and validated panel placement survive reloads; egui memory is not kept.
        fn save(&mut self, storage: &mut dyn eframe::Storage) {
            storage.set_string(cadcraft_ui_egui::options::PROFILE_KEY, self.0.profile_json());
            storage.set_string(cadcraft_ui_egui::PREFS_KEY, self.0.prefs_json());
            match crate::panel_layout::save(&self.0.ui) {
                Ok(layout) => storage.set_string(crate::panel_layout::KEY, layout),
                Err(error) => log::warn!("panel layout was not saved: {error}"),
            }
        }
        fn persist_egui_memory(&self) -> bool {
            false
        }
        /// On the first pass consume files dropped on the page, then delegate drawing to `CadApp::ui`.
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            let first_pass = ui.ctx().current_pass_index() == 0;
            if first_pass {
                // `take_dropped` consumes the raw list; a second pass or hidden tick cannot
                // re-read the same file.
                self.1.take_dropped(ui.ctx());
            }
            self.0.ui(ui);
        }
    }

    /// Writes the browser's document title, which names the tab and is what an embedding page
    /// (`?host=parent`) reads. eframe's web runner ignores [`egui::ViewportCommand::Title`], so
    /// the title has to reach the DOM directly.
    fn set_page_title(title: &str) {
        match web_sys::window().and_then(|window| window.document()) {
            Some(document) => document.set_title(title),
            None => log::warn!("cannot set the page title to {title:?}: there is no document"),
        }
    }

    fn query() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    fn query_param(key: &str) -> Option<String> {
        web_sys::UrlSearchParams::new_with_str(&query()).ok()?.get(key)
    }

    /// 宿主服务：在 OTools 里用宿主的对话框；否则退回上游网页版的行为
    /// （「保存」下载当前文件名，`saveas {"path": "name.dxf"}` 换一个名字）。
    fn services() -> Services {
        if crate::otools::available() {
            let name = |name: &str| crate::otools::pick_save(name);
            Services { pick_save: Some(Box::new(name)), ..Services::default() }
        } else {
            Services { pick_save: Some(Box::new(|name: &str| Some(name.to_string()))), ..Services::default() }
        }
    }

    pub fn start() {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        cadcraft_engine::cmd::file::set_io(cadcraft_engine::cmd::file::IoHooks {
            read: |b, name| cadcraft_io::read(b, name).map_err(|e| e.to_string()),
            write: |d, name| cadcraft_io::write(d, name).map_err(|e| e.to_string()),
            plot: Some(|d, space, opts| cadcraft_io::plotter::plot(d, space, opts).map_err(|e| e.to_string())),
        });
        cadcraft_engine::cmd::file::set_write_as(|d, name, o| {
            cadcraft_io::write_with(d, name, &cadcraft_io::SaveOptions { version: o.version, binary: o.binary }).map_err(|e| e.to_string())
        });
        // AutoLISP 的 entget / entmake / entmod 把实体看作 DXF 组码。
        cadcraft_engine::cmd::file::set_entity_codec(cadcraft_engine::cmd::file::EntityCodec {
            groups: cadcraft_io::entity_groups,
            entity: cadcraft_io::entity_from_groups,
        });
        // 保存：OTools 里交给宿主写盘，否则（纯浏览器预览）退回浏览器下载。
        if crate::otools::available() {
            cadcraft_engine::cmd::file::set_deliver(crate::otools::deliver);
        } else {
            cadcraft_engine::cmd::file::set_deliver(crate::download::download);
        }
        wasm_bindgen_futures::spawn_local(async move {
            let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
            let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
                log::error!("missing <canvas id=\"{CANVAS_ID}\">");
                return;
            };
            let mut options = eframe::WebOptions::default();
            if query().contains("webgl")
                && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
            {
                create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
            }
            let result = eframe::WebRunner::new()
                .start(
                    canvas,
                    options,
                    Box::new(move |cc| {
                        let mut app = CadApp::new(Session::new(), services());
                        // wasm 读不到系统字体：内嵌 CJK + 阿拉伯 fallback，否则中文/日文/阿拉伯文方框。
                        cadcraft_ui_egui::theme::set_extra_fallback_fonts(vec![
                            ("otools-cjk".into(), std::sync::Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/otools-cjk-subset.ttf")))),
                            ("otools-arabic".into(), std::sync::Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/otools-arabic-subset.ttf")))),
                        ]);
                        // 跟随宿主语言：优先用 OTools 注入的 locale，否则退回浏览器 navigator.languages。
                        let host = crate::otools::host_language();
                        app.system_languages = if host.is_empty() {
                            web_sys::window()
                                .map(|w| w.navigator().languages().iter().filter_map(|v| v.as_string()).collect())
                                .unwrap_or_default()
                        } else {
                            vec![host]
                        };
                        if let Some(profile) = cc.storage.and_then(|s| s.get_string(cadcraft_ui_egui::options::PROFILE_KEY)) {
                            app.load_profile(&profile);
                        }
                        match cc.storage.and_then(|s| s.get_string(cadcraft_ui_egui::PREFS_KEY)) {
                            // 用户已经在「选项」里选过主题/语言：以它为准。
                            Some(prefs) => app.load_prefs(&prefs),
                            // 第一次打开：跟随 OTools 的界面主题（插件 webview 的
                            // prefers-color-scheme 不一定等于宿主主题），
                            // 也可以用 URL 的 `?theme=dark|light` 显式覆盖。
                            None => {
                                let theme = match crate::otools::theme() {
                                    t if !t.is_empty() => t,
                                    _ => query_param("theme").unwrap_or_default(),
                                };
                                if let Some(theme) = cadcraft_ui_egui::theme::ThemePref::parse(&theme) {
                                    app.ui.theme = theme;
                                }
                            }
                        }
                        if let Some(layout) = cc.storage.and_then(|s| s.get_string(crate::panel_layout::KEY))
                            && let Err(error) = crate::panel_layout::restore(&mut app.ui, &layout)
                        {
                            log::warn!("panel layout was not restored: {error}");
                        }
                        // `?cpu`: no render state for the app, so the canvas draws with egui shapes.
                        if !crate::wants_cpu(&query())
                            && let Some(rs) = &cc.wgpu_render_state
                        {
                            // eframe 的 web runner 没有模板缓冲：被裁剪的布局视口留在布局的显示列表里。
                            app.set_wgpu(rs, None);
                        }
                        if query().contains("sample") {
                            let _ = app.run("ui.sample", serde_json::json!({}));
                        }
                        // 没有路径可用：OPEN 由 pick_open 发起，读到的字节在后台送进 inbox。
                        let inbox = crate::upload::Inbox::new(&cc.egui_ctx);
                        crate::otools::install(inbox.clone());
                        // 宿主字体桥：取一次系统字体目录（异步，到达后字体下拉即可列出）。
                        crate::fontbridge::fetch_catalog();
                        if crate::otools::available() {
                            app.services.pick_open = Some(Box::new(crate::otools::pick_open));
                        } else {
                            let picker = inbox.clone();
                            app.services.pick_open = Some(Box::new(move || {
                                picker.pick();
                                None
                            }));
                        }
                        let reduced_motion = web_sys::window().and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten());
                        Ok(Box::new(Shell(app, inbox, reduced_motion)))
                    }),
                )
                .await;
            if let Some(el) = document.get_element_by_id(LOADING_ID) {
                match result {
                    Ok(()) => el.remove(),
                    Err(e) => {
                        el.set_inner_html(&format!("<p>CADCraft 启动失败：{e:?}</p><p>需要支持 WebGPU 或 WebGL2 的浏览器。</p>"))
                    }
                }
            }
            install_locale_listener();
        });
    }

    /// 监听宿主语言切换（OTools 注入的 `otools-locale-changed` 事件），置位标志并请求重绘；
    /// 真正更新 `system_languages` 发生在下一帧 `Shell::logic`。
    fn install_locale_listener() {
        let Some(window) = web_sys::window() else { return };
        let closure = Closure::wrap(Box::new(|| {
            NEEDS_LANG_SYNC.with(|f| f.set(true));
            if let Some(ctx) = APP_CTX.with(|c| c.borrow().clone()) {
                ctx.request_repaint();
            }
        }) as Box<dyn FnMut()>);
        let _ = window.add_event_listener_with_callback(
            "otools-locale-changed",
            closure.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        // 宿主投递了文件：同一份「置位 + 请求重绘」逻辑，让下一帧的 logic 去取件。
        let _ = window.add_event_listener_with_callback(
            "otools-open-files-arrived",
            closure.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        closure.forget();
    }
}

/// URL 查询串（`?cpu&sample`）里有 `cpu` 标志时为真：请求 CPU 画布路径。
#[cfg(any(target_arch = "wasm32", test))]
fn wants_cpu(query: &str) -> bool {
    query.trim_start_matches('?').split('&').any(|p| p == "cpu" || p.starts_with("cpu="))
}

#[cfg(target_arch = "wasm32")]
mod upload;

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("otools-cadcraft 只在浏览器里运行：在 shell/ 目录用 `trunk build --release` 构建");
}

#[cfg(test)]
mod tests {
    use super::wants_cpu;

    #[test]
    fn cpu_flag_is_detected() {
        assert!(wants_cpu("?cpu"));
        assert!(wants_cpu("?cpu&sample"));
        assert!(wants_cpu("?sample&cpu"));
        assert!(!wants_cpu(""));
        assert!(!wants_cpu("?sample"));
        assert!(!wants_cpu("?webgl&sample"));
        assert!(!wants_cpu("?gpucpu"));
    }
}
