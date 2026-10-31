//! 阅读窗 HTML 原样渲染（macOS：wry 网页子视图挂 iced 主窗）
//!
//! ## 职责与边界
//!
//! - **原样渲染**：消毒后的 HTML 交给系统 WKWebView 画（css/表格/内联图全保留）
//! - **安全纵深**（三层互不依赖，此处管两层）：
//!   1. 消毒白名单（smail-core::sanitized_html_for_webview：删 script/危险协议）
//!   2. 本模块禁 JS + 拦全部导航（链接经点击脚本转系统浏览器打开）
//!   3. 远程图片默认禁（跟踪像素），「显示图片」显式放行
//! - **几何跟随**：阅读窗正文区由探针（drag.rs::probe_pane）每帧回填 bounds，
//!   窗口缩放/列宽变化时 set_bounds 同步（逻辑点坐标，wry 内部按缩放系数换算）
//!
//! ## 线程模型
//!
//! wry::WebView 非 Send，只能活在主线程——iced 的 update/view 都在主线程跑，
//! 本体存 State 字段即可。原生窗口句柄经 `window::run` 回调取原始指针（u64，
//! Send 安全），重建句柄的动作只发生在主线程 update 里，主窗存活期内有效。
//!
//! 非 macOS 平台：依赖不引入，全部函数为空实现（按钮入口在视图层一并隐藏）。

use iced::Task;

use crate::{Message, State};

/// 切换原样渲染开关（阅读器工具栏按钮）
pub(crate) fn toggle(state: &mut State, acct: usize, hash: i64) -> Task<Message> {
    #[cfg(target_os = "macos")]
    {
        state.html.on = !state.html.on;
        if !state.html.on {
            close(state);
            return Task::none();
        }
        // 开：同步抽取+消毒正文（本地缓存 .eml，毫秒级；Resend 同模式），
        // 随后创建/装载网页视图
        state.html.load_remote = false;
        if let Err(e) = apply(state, acct, hash) {
            state.html.on = false;
            state.error = Some(e);
            return Task::none();
        }
        ensure_handles(state)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, acct, hash);
        Task::none()
    }
}

/// 阅读对象切换时刷新内容（BodyLoaded 回调；开关开着才动）
pub(crate) fn apply_reading(state: &mut State, acct: usize, hash: i64) {
    #[cfg(target_os = "macos")]
    {
        if !state.html.on {
            return;
        }
        if let Err(e) = apply(state, acct, hash) {
            // 非 HTML 邮件（无正文 HTML）不算错误：隐藏视图回退 markdown
            state.html.source = None;
            if let Some(wv) = state.webview.as_ref() {
                let _ = wv.set_visible(false);
            }
            let _ = e;
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, acct, hash);
    }
}

/// 用户点「显示图片」：重消毒放行远程图并重新装载（不用 markdown 视图的
/// 下载通道——网页视图自己会拉图）
pub(crate) fn remote_reveal(state: &mut State, acct: usize, hash: i64) {
    #[cfg(target_os = "macos")]
    {
        state.html.load_remote = true;
        if let Err(e) = apply(state, acct, hash) {
            state.error = Some(e);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, acct, hash);
    }
}

/// 关闭：丢掉子视图（Drop 自动从父视图摘除）并复位会话状态
pub(crate) fn close(state: &mut State) {
    #[cfg(target_os = "macos")]
    {
        state.webview = None;
        state.html.on = false;
        state.html.source = None;
        state.html.synced = None;
        state.html.pending_open = false;
        state.html.handles_ready = false;
        state.html.raw_ns_view = None;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state;
    }
}

/// 消毒正文抽取+装载（同步；失败返回用户可读原因）
#[cfg(target_os = "macos")]
fn apply(state: &mut State, acct: usize, hash: i64) -> Result<(), String> {
    let slug = state
        .accounts
        .get(acct)
        .map(|a| a.slug.clone())
        .unwrap_or_default();
    let path = state
        .data_root
        .join(&slug)
        .join("bodies")
        .join(format!("{hash}.eml"));
    let bytes = std::fs::read(&path).map_err(|e| format!("读原信缓存失败：{e}"))?;
    let msg = mail_parser::MessageParser::default()
        .parse(&bytes)
        .ok_or_else(|| "原信解析失败".to_string())?;
    let html = msg.body_html(0).map(|h| h.into_owned());
    let Some(html) = html else {
        state.html.source = None;
        state.html.synced = None;
        hide(state);
        return Err("这封邮件没有 HTML 正文".into());
    };
    let sanitized = smail_core::sanitized_html_for_webview(&html, state.html.load_remote);
    state.html.source = Some(sanitized);
    state.html.synced = None;
    // 已就绪：装载到现有视图；未就绪：标记挂起（handles_ready 回调后补创建）
    if state.webview.is_some() {
        if let (Some(wv), Some(src)) = (state.webview.as_ref(), state.html.source.as_ref()) {
            wv.load_html(src).map_err(|e| e.to_string())?;
            let _ = wv.set_visible(true);
        }
    } else if state.html.handles_ready {
        if let Err(e) = create(state) {
            state.html.on = false;
            return Err(e);
        }
    } else {
        state.html.pending_open = true;
    }
    sync_bounds(state);
    Ok(())
}

#[cfg(target_os = "macos")]
fn hide(state: &mut State) {
    if let Some(wv) = state.webview.as_ref() {
        let _ = wv.set_visible(false);
    }
}

/// 捕获主窗原生句柄（window::run 异步回调；指针以 u64 运送——RawWindowHandle
/// 非 Send，整数 transport 是合法跨线程形态，重建只发生在主线程）
#[cfg(target_os = "macos")]
pub(crate) fn ensure_handles(state: &mut State) -> Task<Message> {
    if state.html.handles_ready {
        return Task::none();
    }
    let Some(id) = state.main_window else {
        state.html.on = false;
        state.error = Some("主窗未就绪，无法开启原样渲染".into());
        return Task::none();
    };
    iced::window::run(id, |w| match w.window_handle() {
        Ok(h) => match h.as_raw() {
            wry::raw_window_handle::RawWindowHandle::AppKit(k) => k.ns_view.as_ptr() as u64,
            _ => 0,
        },
        Err(_) => 0,
    })
    .map(Message::HtmlHandlesReady)
}

/// 句柄回填：指针 0=失败；否则建视图（挂起时）并同步几何
pub(crate) fn handles_ready(state: &mut State, ns_view: u64) -> Task<Message> {
    #[cfg(target_os = "macos")]
    {
        state.html.handles_ready = true;
        if ns_view == 0 {
            close(state);
            state.error = Some("无法获取主窗原生句柄，原样渲染不可用".into());
            return Task::none();
        }
        state.html.raw_ns_view = Some(ns_view);
        if state.html.pending_open && state.html.source.is_some() {
            state.html.pending_open = false;
            if let Err(e) = create(state) {
                close(state);
                state.error = Some(e);
            }
            sync_bounds(state);
        }
        Task::none()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (state, ns_view);
        Task::none()
    }
}

/// 阅读窗几何变化后同步子视图边界（有变化才调系统接口）
pub(crate) fn sync_bounds(state: &mut State) {
    #[cfg(target_os = "macos")]
    {
        let Some(rect) = state.html.pane.borrow().last().map(|(r, _)| *r) else {
            return;
        };
        let key = [rect.x, rect.y, rect.width, rect.height];
        if state.html.synced.as_ref() == Some(&key) {
            return;
        }
        if let Some(wv) = state.webview.as_ref() {
            let _ = wv.set_bounds(wry::Rect {
                position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(
                    rect.x as f64,
                    rect.y as f64,
                )),
                size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(
                    rect.width as f64,
                    rect.height as f64,
                )),
            });
            state.html.synced = Some(key);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state;
    }
}

/// 创建网页子视图（挂在主窗内容视图上；update 主线程调用）
#[cfg(target_os = "macos")]
fn create(state: &mut State) -> Result<(), String> {
    use wry::raw_window_handle::{HasWindowHandle, WindowHandle};

    if state.webview.is_some() {
        return Ok(());
    }
    let Some(ns_view) = state.html.raw_ns_view else {
        return Err("原生窗口句柄未捕获".into());
    };
    let Some(src) = state.html.source.clone() else {
        return Err("HTML 尚未就绪".into());
    };
    let dark = matches!(state.prefs.ui_theme, iced::Theme::Dark);
    // 底色与主题卡片一致：子视图首帧出现前不闪白
    let bg: (u8, u8, u8, u8) = if dark {
        (33, 33, 38, 255)
    } else {
        (255, 255, 250, 255)
    };

    struct Parent {
        ns_view: u64,
    }
    impl HasWindowHandle for Parent {
        fn window_handle(&self) -> Result<WindowHandle<'_>, wry::raw_window_handle::HandleError> {
            let ptr = std::ptr::NonNull::new(self.ns_view as *mut std::ffi::c_void)
                .expect("主窗 ns_view 指针失效");
            let raw = wry::raw_window_handle::RawWindowHandle::AppKit(
                wry::raw_window_handle::AppKitWindowHandle::new(ptr),
            );
            // SAFETY：指针来自 window::run 期 iced 主窗内容视图，且本函数只在
            // 主线程 update 执行；主窗关闭路径先经 close() 丢掉 webview
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    let pane = state.html.pane.borrow().last().map(|(r, _)| *r);
    let bounds = pane
        .map(|r| wry::Rect {
            position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(
                r.x as f64, r.y as f64,
            )),
            size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(
                r.width as f64,
                r.height as f64,
            )),
        })
        .unwrap_or_default();

    let parent = Parent { ns_view };
    let view = wry::WebViewBuilder::new()
        .with_bounds(bounds)
        .with_html(src)
        .with_incognito(true)
        .with_javascript_disabled()
        .with_autoplay(false)
        .with_back_forward_navigation_gestures(false)
        .with_accept_first_mouse(true)
        .with_background_color(bg)
        // 视图内导航一律取消（初始 with_html 不走该回调）；链接由
        // 初始化脚本截获后转系统浏览器（见下），iframe 追踪不会外弹
        .with_navigation_handler(|_url| false)
        .with_new_window_req_handler(|_url, _features| wry::NewWindowResponse::Deny)
        .with_initialization_script(
            r#"document.addEventListener('click', function (e) {
  var el = e.target;
  while (el && el.nodeName !== 'A') { el = el.parentElement; }
  if (el && el.href) {
    e.preventDefault();
    window.ipc.postMessage(el.href);
  }
}, true);"#,
        )
        .with_ipc_handler(|req| {
            let url = req.body().clone();
            crate::open_in_browser(&url);
        })
        .build_as_child(&parent)
        .map_err(|e| format!("创建网页视图失败：{e}"))?;
    state.webview = Some(view);
    Ok(())
}
