//! macOS 通知：mac-notification-sys（原生 UNUserNotificationCenter）。
//!
//! 点击跳转 v1：通知发出时记录最近一封新邮件的位置；通知点击会激活
//! app（macOS 默认行为）→ tray 激活路径消费记录 → 跳转到该邮件。
//! v2 接 UNUserNotificationCenter delegate 回调可精确到每封邮件（需 .app）。

use std::sync::Mutex;

/// 最近的邮件通知（点击通知 → 激活 app → 跳转到这封）
static LAST_MAIL: Mutex<Option<NotifyClick>> = Mutex::new(None);

/// 通知点击载荷：(账号, 文件夹, hash)
#[derive(Debug, Clone)]
pub(crate) struct NotifyClick {
    pub(crate) acct: usize,
    pub(crate) folder: String,
    pub(crate) hash: i64,
}

/// 取出最近的邮件通知（消费一次即清空——tray 激活或通知点击时调用）
pub(crate) fn take_last() -> Option<NotifyClick> {
    LAST_MAIL.lock().ok().and_then(|mut g| g.take())
}

/// 新邮件通知：mac-notification-sys 发出 + 记录位置供跳转
pub(crate) fn notify_mail(from: &str, subject: &str, click: NotifyClick) {
    if std::env::var_os("SMAIL_NO_NOTIFY").is_some() {
        return;
    }
    // 记录最近通知的邮件（tray 激活时跳转用）
    if let Ok(mut g) = LAST_MAIL.lock() {
        *g = Some(click);
    }
    #[cfg(target_os = "macos")]
    {
        // 简单通知（无 action 按钮 → 无生命周期问题）；点击激活 app
        //（macOS 默认：通知点击 → 激活 bundle id 对应的进程）
        let _ = mac_notification_sys::send_notification(from, None, subject, None);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (from, subject);
    }
}
