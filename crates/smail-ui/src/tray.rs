//! 系统托盘（macOS v1）：常驻图标 + 菜单（打开 / 退出）。
//!
//! - Linux 需要 gtk/appindicator 运行时，暂不启用（依赖按平台限定，
//!   Linux/Windows 不编译 tray-icon）
//! - 「打开」与左键点击用 System Events 把自己进程拉到前台
//!   （iced 0.14 无窗口句柄 API，gain_focus 需要窗口 Id 且单窗口 Id 不可知）
//! - 托盘实例与菜单事件 handler 均为进程级全局，与 App 同生命周期

use std::sync::mpsc::Receiver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    Quit,
}

static TRAY_RX: std::sync::OnceLock<std::sync::Mutex<Receiver<TrayAction>>> =
    std::sync::OnceLock::new();

/// 订阅入口：读全局接收端（main 启动时已 spawn）
pub fn take_receiver() -> Option<&'static std::sync::Mutex<Receiver<TrayAction>>> {
    TRAY_RX.get()
}

/// 启动托盘（main 最早调用；失败静默降级为无托盘）
#[allow(unused_variables)]
pub fn spawn() {
    #[cfg(target_os = "macos")]
    {
        let rx = spawn_macos();
        if let Some(rx) = rx {
            let _ = TRAY_RX.set(std::sync::Mutex::new(rx));
        }
    }
}

#[cfg(target_os = "macos")]
fn spawn_macos() -> Option<Receiver<TrayAction>> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{MouseButton, TrayIconBuilder, TrayIconEvent};

    let (tx, rx) = std::sync::mpsc::channel::<TrayAction>();
    let tx_click = tx.clone();

    let menu = Menu::new();
    let open = MenuItem::with_id("open", "打开 s-mail", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    menu.append_items(&[&open, &quit]).ok()?;

    let icon = make_icon()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("s-mail")
        .with_icon(icon)
        .build()
        .ok()?;
    // 托盘与进程同生命周期：有意泄漏（drop 会移除托盘图标）
    std::mem::forget(tray);

    MenuEvent::set_event_handler(Some(move |ev: tray_icon::menu::MenuEvent| {
        match ev.id().0.as_str() {
            "open" => {
                let _ = tx.send(TrayAction::Open);
            }
            "quit" => {
                let _ = tx.send(TrayAction::Quit);
            }
            _ => {}
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
        // 左键点击 = 打开（macOS 托盘默认不激活窗口）
        if let TrayIconEvent::Click { button, .. } = ev {
            if button == MouseButton::Left {
                let _ = tx_click.send(TrayAction::Open);
            }
        }
    }));
    Some(rx)
}

/// 32x32 程序生成图标：蓝色圆角方块 + 白色信封（无外部资源依赖）
#[cfg(target_os = "macos")]
fn make_icon() -> Option<tray_icon::Icon> {
    const W: u32 = 32;
    const H: u32 = 32;
    let mut rgba = vec![0u8; (W * H * 4) as usize];
    let put = |rgba: &mut [u8], x: i32, y: i32, r: u8, g: u8, b: u8| {
        if (0..W as i32).contains(&x) && (0..H as i32).contains(&y) {
            let i = ((y * W as i32 + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    };
    // 蓝底（简单圆角：四角留空）
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let near_l = x < 3;
            let near_r = x > W as i32 - 4;
            let near_t = y < 3;
            let near_b = y > H as i32 - 4;
            let corner = (near_l || near_r) && (near_t || near_b);
            if !corner {
                put(&mut rgba, x, y, 41, 128, 255);
            }
        }
    }
    // 白色信封主体
    for y in 10..22 {
        for x in 6..26 {
            put(&mut rgba, x, y, 255, 255, 255);
        }
    }
    // 信封盖折线（蓝）
    for t in 0..10 {
        put(&mut rgba, 6 + t, 10 + t, 41, 128, 255);
        put(&mut rgba, 25 - t, 10 + t, 41, 128, 255);
    }
    tray_icon::Icon::from_rgba(rgba, W, H).ok()
}
