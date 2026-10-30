//! 系统通知（macOS）
//!
//! ## 职责
//!
//! - **新邮件通知**：收到新邮件时发出系统通知
//! - **点击跳转**：点击通知 → 激活 app → 跳转到对应邮件
//! - **通知队列**：FIFO 队列管理多封新邮件通知
//!
//! ## 技术实现
//!
//! - **后端**：mac-notification-sys（原生 UNUserNotificationCenter）
//! - **通知内容**：发件人 + 主题
//! - **点击载荷**：(账号, 文件夹, hash)
//!
//! ## 跳转机制（v2）
//!
//! ```text
//! 新邮件到达 → notify_mail() 发出通知 + 记录载荷
//!   ↓
//! 用户点击通知 → macOS 激活 app
//!   ↓
//! tray 激活路径 → take_last() 消费队列
//!   ↓
//! 跳转到对应邮件（账号/文件夹/hash）
//! ```
//!
//! - **v1**：单条记录，只能跳转最后一封
//! - **v2**：FIFO 队列，多封新邮件逐次激活逐封跳转
//!
//! ## 环境控制
//!
//! - `SMAIL_NO_NOTIFY` 环境变量：禁用通知（测试/调试用）

use std::collections::VecDeque;
use std::sync::Mutex;

/// 邮件通知队列（点击通知 → 激活 app → 跳转到最早未消费的一封；
/// 多封新邮件同时到 → 逐次激活逐封跳转——v2 从单条改为 FIFO 队列）
static MAIL_QUEUE: Mutex<VecDeque<NotifyClick>> = Mutex::new(VecDeque::new());

/// 通知点击载荷：(账号, 文件夹, hash)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyClick {
    pub(crate) acct: usize,
    pub(crate) folder: String,
    pub(crate) hash: i64,
}

/// 取出最早未消费的邮件通知（tray 激活或通知点击时调用；逐次消费）
pub(crate) fn take_last() -> Option<NotifyClick> {
    MAIL_QUEUE.lock().ok().and_then(|mut g| g.pop_front())
}

/// 新邮件通知：mac-notification-sys 发出 + 记录位置供跳转
pub(crate) fn notify_mail(from: &str, subject: &str, click: NotifyClick) {
    if std::env::var_os("SMAIL_NO_NOTIFY").is_some() {
        return;
    }
    // 入队（FIFO；上限 10 防无限膨胀）
    if let Ok(mut g) = MAIL_QUEUE.lock() {
        if g.len() < 10 {
            g.push_back(click);
        }
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
