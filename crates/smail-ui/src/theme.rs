//! UI 主题（Apple 风格基础）：macOS 系统色 + 语义化样式函数。
//!
//! 设计原则（对应 apple-design 技能）：
//! - 色彩层级：主文本 / 次文本（Apple secondaryLabel）/ 强调色（systemBlue）
//! - 材质感：侧栏用更重的底色分离结构区（无 backdrop-blur，用色块近似）
//! - 选中态：强调色 15% 圆角色块（非边框），hover 4% —— 无硬分隔线
//! - 徽章语义：成功=绿、失败=红、未验证=灰（色弱可辨：同时有符号前缀）
//! - 动画克制：iced 无弹簧系统，不造假动效；反馈靠即时状态变化

use iced::widget::{button, container, text};
use iced::{Color, Font, Theme};

// ---------------------------------------------------------------------------
// 调色板（macOS system colors）
// ---------------------------------------------------------------------------

pub fn accent(t: &Theme) -> Color {
    match t {
        Theme::Dark => Color::from_rgb(0.04, 0.52, 1.0), // systemBlue dark
        _ => Color::from_rgb(0.0, 0.48, 1.0),            // systemBlue #007AFF
    }
}

pub fn accent_tint(t: &Theme) -> Color {
    match t {
        Theme::Dark => Color::from_rgba(0.04, 0.52, 1.0, 0.30),
        _ => Color::from_rgba(0.0, 0.48, 1.0, 0.14),
    }
}

pub fn hover_tint(t: &Theme) -> Color {
    match t {
        Theme::Dark => Color::from_rgba(1.0, 1.0, 1.0, 0.11),
        _ => Color::from_rgba(0.0, 0.0, 0.0, 0.05),
    }
}

pub fn secondary_text_color(t: &Theme) -> Color {
    match t {
        // 0.60 在深底上 11px 小字对比度不足（用户反馈“看不清”），提亮到 0.68
        Theme::Dark => Color::from_rgb(0.68, 0.68, 0.71),
        _ => Color::from_rgb(0.43, 0.43, 0.45), // #6E6E73
    }
}

fn gold() -> Color {
    Color::from_rgb(1.0, 0.72, 0.15)
}

/// 主题感知成功色：深色用 systemGreen 亮变体（原浅色值在深底上发暗）
fn success(t: &Theme) -> Color {
    match t {
        Theme::Dark => Color::from_rgb(0.19, 0.82, 0.36),
        _ => Color::from_rgb(0.14, 0.69, 0.30),
    }
}

/// 主题感知危险色：深色用 systemRed 亮变体
fn danger(t: &Theme) -> Color {
    match t {
        Theme::Dark => Color::from_rgb(1.0, 0.42, 0.38),
        _ => Color::from_rgb(1.0, 0.27, 0.23),
    }
}

// ---------------------------------------------------------------------------
// 样式函数
// ---------------------------------------------------------------------------

/// 次要文本（日期、发件人、账号状态）
pub fn secondary_text(t: &Theme) -> text::Style {
    text::Style {
        color: Some(secondary_text_color(t)),
    }
}

/// 顶栏：半透明底 + 底缘细线（近似 translucent toolbar）
pub fn toolbar_bg(t: &Theme) -> container::Style {
    container::Style {
        background: Some(
            match t {
                Theme::Dark => Color::from_rgba(0.12, 0.12, 0.13, 0.85),
                _ => Color::from_rgba(0.96, 0.96, 0.97, 0.9),
            }
            .into(),
        ),
        border: iced::Border {
            color: match t {
                Theme::Dark => Color::from_rgba(1.0, 1.0, 1.0, 0.08),
                _ => Color::from_rgba(0.0, 0.0, 0.0, 0.10),
            },
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

/// 侧栏：比重底色分离结构区（材质感）
pub fn sidebar_bg(t: &Theme) -> container::Style {
    container::Style {
        background: Some(match t {
            Theme::Dark => Color::from_rgb(0.11, 0.11, 0.12).into(),
            _ => Color::from_rgb(0.92, 0.92, 0.94).into(),
        }),
        border: iced::Border {
            color: match t {
                Theme::Dark => Color::from_rgba(1.0, 1.0, 1.0, 0.06),
                _ => Color::from_rgba(0.0, 0.0, 0.0, 0.08),
            },
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

/// 列表行：透明默认 / hover 4% / 选中强调色圆角。
/// text_color 必须显式取主题色：iced 的 button::Style::default() 硬编码
/// BLACK（深色模式下黑字黑底的根因）
pub fn list_row(t: &Theme, status: button::Status, selected: bool) -> button::Style {
    let base = button::Style {
        background: None,
        text_color: t.palette().text,
        border: iced::border::rounded(8),
        ..Default::default()
    };
    match (selected, status) {
        (true, _) => button::Style {
            background: Some(accent_tint(t).into()),
            ..base
        },
        (false, button::Status::Hovered) => button::Style {
            background: Some(hover_tint(t).into()),
            ..base
        },
        (false, _) => base,
    }
}

/// 强调按钮（写邮件/发送）：实心圆角胶囊
pub fn accent_button(t: &Theme, _status: button::Status) -> button::Style {
    button::Style {
        background: Some(accent(t).into()),
        text_color: Color::WHITE,
        border: iced::border::rounded(8),
        ..Default::default()
    }
}

/// 幽灵按钮（工具条次要动作）：无底、hover 轻染；
/// 同样显式取主题文本色（default 是 BLACK）
pub fn ghost_button(t: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered => Some(hover_tint(t).into()),
            _ => None,
        },
        text_color: t.palette().text,
        border: iced::border::rounded(7),
        ..Default::default()
    }
}

/// 侧栏文件夹按钮：选中=强调 15% 圆角；未选中透明
/// （text_color 同理由 default 的 BLACK 改为主题色）
pub fn sidebar_item(t: &Theme, status: button::Status, selected: bool) -> button::Style {
    let radius = iced::border::rounded(6);
    match (selected, status) {
        (true, _) => button::Style {
            background: Some(accent_tint(t).into()),
            text_color: t.palette().text,
            border: radius,
            ..Default::default()
        },
        (false, button::Status::Hovered) => button::Style {
            background: Some(hover_tint(t).into()),
            text_color: t.palette().text,
            border: radius,
            ..Default::default()
        },
        (false, _) => button::Style {
            text_color: t.palette().text,
            border: radius,
            ..Default::default()
        },
    }
}

/// PGP / 状态徽章：语义着色圆角块（内容自带符号，色弱可辨）
pub fn status_badage(t: &Theme, kind: BadgeKind) -> container::Style {
    let (fg, bg) = match kind {
        BadgeKind::Good => {
            let c = success(t);
            let a = if matches!(t, Theme::Dark) { 0.20 } else { 0.12 };
            (c, Color::from_rgba(c.r, c.g, c.b, a))
        }
        BadgeKind::Bad => {
            let c = danger(t);
            let a = if matches!(t, Theme::Dark) { 0.20 } else { 0.12 };
            (c, Color::from_rgba(c.r, c.g, c.b, a))
        }
        BadgeKind::Warn => (
            secondary_text_color(t),
            match t {
                Theme::Dark => Color::from_rgba(1.0, 1.0, 1.0, 0.10),
                _ => Color::from_rgba(0.0, 0.0, 0.0, 0.06),
            },
        ),
    };
    container::Style {
        background: Some(bg.into()),
        text_color: Some(fg),
        border: iced::border::rounded(6),
        ..Default::default()
    }
}

pub enum BadgeKind {
    Good,
    Bad,
    Warn,
}

/// 从 PGP 状态文本推断徽章语义
pub fn badge_kind(status: &str) -> BadgeKind {
    if status.contains('✓') || status.contains("已解密") {
        BadgeKind::Good
    } else if status.contains('✗') {
        BadgeKind::Bad
    } else {
        BadgeKind::Warn
    }
}

/// 系统字体（SF Pro Text 回退默认）+ 字重
pub fn ui_font(weight: iced::font::Weight) -> Font {
    Font {
        weight,
        ..Font::with_name("SF Pro Text")
    }
}

/// 未读数胶囊（强调色淡底 + 强调色文字）
pub fn unread_pill(t: &Theme) -> container::Style {
    container::Style {
        background: Some(accent_tint(t).into()),
        text_color: Some(accent(t)),
        border: iced::border::rounded(9),
        ..Default::default()
    }
}

/// 星标色
pub fn star_color() -> Color {
    gold()
}

/// 阅读区信头分隔：极淡底或无
pub fn reader_meta(t: &Theme) -> container::Style {
    container::Style {
        background: Some(
            match t {
                Theme::Dark => Color::from_rgba(1.0, 1.0, 1.0, 0.04),
                _ => Color::from_rgba(0.0, 0.0, 0.0, 0.03),
            }
            .into(),
        ),
        border: iced::border::rounded(8),
        ..Default::default()
    }
}
