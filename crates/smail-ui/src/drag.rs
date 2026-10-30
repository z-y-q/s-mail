//! 列表行拖拽移动到文件夹（iced 无内建拖拽，自实现手势）
//!
//! ## 职责
//!
//! - **命中矩形登记**：`hit_rect` 包装组件在绘制期把自身边界与身份写入共享表
//!   （列表行 → 可拖源；侧栏文件夹 → 投放目标），按帧刷新，无状态残留
//! - **命中测试**：按下/移动/松开三个时机查表判定拖拽对象与投放目标
//! - **幽灵浮层**：拖动时跟随光标的半透明摘要条（canvas 覆盖层绘制）
//!
//! ## 手势状态机
//!
//! ```text
//! 行按下 DragPress（登记候选，原点击消息随行携带）
//!   → 移动超阈值（6px）→ active=true，幽灵浮现，文件夹高亮
//!   → 松开 DragReleased：
//!       active=false → 还原为普通点击（转发原消息，点击行为不变）
//!       active=true 且悬停目标 → 发移动消息（多选时整批移动）
//!       active=true 无目标 → 取消（拖回原处=无操作）
//! Esc 随时取消
//! ```
//!
//! 全局鼠标流订阅仅在有候选时开启（与分割条拖拽同一模式，平时零开销）。

use std::cell::RefCell;
use std::rc::Rc;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{self, Operation, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::widget::canvas;
use iced::{Element, Length, Point, Rectangle, Size, Vector};

/// 可拖拽的邮件行（登记期快照；移动命令所需字段齐备）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DragSrc {
    pub(crate) acct: usize,
    pub(crate) hash: i64,
    /// 拖拽源文件夹（多选整批移动时排除目标=源文件夹）
    pub(crate) folder: String,
    /// 幽灵浮层显示的主题
    pub(crate) subject: String,
}

/// 投放目标：侧栏某个文件夹行
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DropTarget {
    pub(crate) acct: usize,
    pub(crate) path: String,
    /// 侧栏本地化名（幽灵提示用）
    pub(crate) label: String,
}

/// 侧栏文件夹命中表（每帧绘制时回填，view 构建前清空）
pub(crate) type TargetRects = Rc<RefCell<Vec<(Rectangle, DropTarget)>>>;
/// 阅读窗几何探针表（原样渲染视图定位原生网页视图用）
pub(crate) type PaneRects = Rc<RefCell<Vec<(Rectangle, ())>>>;

pub(crate) fn new_target_rects() -> TargetRects {
    Rc::new(RefCell::new(Vec::new()))
}

pub(crate) fn new_pane_rects() -> PaneRects {
    Rc::new(RefCell::new(Vec::new()))
}

/// 点是否离起点足够远（判定「拖」与「点」；6px 与系统拖拽阈值惯例一致）
pub(crate) fn beyond_drag_threshold(press: Point, cursor: Point) -> bool {
    let dx = cursor.x - press.x;
    let dy = cursor.y - press.y;
    dx * dx + dy * dy > 36.0
}

/// 命中测试：光标落在哪个侧栏文件夹上
pub(crate) fn target_at(rects: &[(Rectangle, DropTarget)], p: Point) -> Option<DropTarget> {
    rects
        .iter()
        .find(|(r, _)| r.contains(p))
        .map(|(_, t)| t.clone())
}

/// 拖拽中的邮件（候选期=已按下未超阈值；active=拖拽进行中）
#[derive(Debug, Clone)]
pub(crate) struct MailDrag {
    pub(crate) src: DragSrc,
    /// 按下点（首条 CursorMoved 回填——on_press 不带坐标）
    pub(crate) press: Option<Point>,
    pub(crate) cursor: Option<Point>,
    pub(crate) active: bool,
    /// 悬停中的投放目标（高亮 + 幽灵提示）
    pub(crate) hover: Option<DropTarget>,
    /// 原点击消息：未成拖拽时松开即还原为普通点击
    pub(crate) click: std::boxed::Box<crate::Message>,
}

impl MailDrag {
    pub(crate) fn new(
        acct: usize,
        hash: i64,
        folder: String,
        subject: String,
        click: crate::Message,
    ) -> Self {
        Self {
            src: DragSrc {
                acct,
                hash,
                folder,
                subject,
            },
            press: None,
            cursor: None,
            active: false,
            hover: None,
            click: Box::new(click),
        }
    }
}

/// 边界登记包装组件：绘制期把自身 bounds 连同标记物压入共享表。
/// 除 draw 外全部委托内层（树处理照 iced 自家 mouse_area 的模式）。
/// 标记物泛型：拖投放目标用 DropTarget，阅读窗几何探针用 ()
pub(crate) struct HitRect<'a, T, Message, Theme, Renderer>
where
    T: Clone,
{
    inner: Element<'a, Message, Theme, Renderer>,
    sink: Rc<RefCell<Vec<(Rectangle, T)>>>,
    item: T,
}

/// 包一层投放目标登记（侧栏文件夹行用）
pub(crate) fn hit_target<'a, Message, Theme, Renderer>(
    inner: impl Into<Element<'a, Message, Theme, Renderer>>,
    sink: TargetRects,
    item: DropTarget,
) -> Element<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer + 'static,
    Message: 'a,
    Theme: 'a,
{
    Element::new(HitRect {
        inner: inner.into(),
        sink,
        item,
    })
}

/// 包一层几何探针（draw 时回填自身 bounds；阅读窗原样渲染区定位用）
pub(crate) fn probe_pane<'a, Message, Theme, Renderer>(
    inner: impl Into<Element<'a, Message, Theme, Renderer>>,
    sink: Rc<RefCell<Vec<(Rectangle, ())>>>,
) -> Element<'a, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer + 'static,
    Message: 'a,
    Theme: 'a,
{
    Element::new(HitRect {
        inner: inner.into(),
        sink,
        item: (),
    })
}

impl<T, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for HitRect<'_, T, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
    T: Clone,
{
    fn size(&self) -> Size<Length> {
        self.inner.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.inner.as_widget().size_hint()
    }

    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::stateless()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::None
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.inner)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.inner));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.inner
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.inner
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.inner.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> iced::mouse::Interaction {
        self.inner.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        self.sink.borrow_mut().push((bounds, self.item.clone()));
        self.inner.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<iced::advanced::overlay::Element<'a, Message, Theme, Renderer>> {
        self.inner.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// 拖拽幽灵浮层：跟随光标的半透明摘要条（canvas 覆盖层，不拦事件）
pub(crate) struct GhostChip {
    pub(crate) at: Option<Point>,
    pub(crate) subject: String,
    pub(crate) n_extra: usize,
    pub(crate) dest: Option<String>,
    pub(crate) dark: bool,
}

impl<Message> canvas::Program<Message> for GhostChip {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        if let Some(p) = self.at {
            let mut lines = vec![truncate_chars(&self.subject, 32)];
            if self.n_extra > 0 {
                lines.push(format!("等 {} 封", self.n_extra + 1));
            }
            if let Some(d) = &self.dest {
                lines.push(format!("→ {d}"));
            }
            let line_h = 18.0;
            let pad = 8.0;
            let w =
                lines.iter().map(|l| l.chars().count()).max().unwrap_or(8) as f32 * 6.5 + pad * 2.0;
            let h = lines.len() as f32 * line_h + pad * 2.0;
            // 光标右下偏移，别挡住落点本身
            let chip = Point::new(p.x + 14.0, p.y + 14.0);
            let bg = if self.dark {
                iced::Color::from_rgba(0.22, 0.22, 0.25, 0.94)
            } else {
                iced::Color::from_rgba(1.0, 1.0, 1.0, 0.94)
            };
            let border = if self.dest.is_some() {
                iced::Color::from_rgb(0.20, 0.48, 0.90)
            } else {
                iced::Color::from_rgba(0.5, 0.5, 0.55, 0.6)
            };
            frame.fill_rectangle(chip, Size::new(w, h), bg);
            frame.stroke_rectangle(
                chip,
                Size::new(w, h),
                canvas::Stroke {
                    style: canvas::Style::Solid(border),
                    width: 1.5,
                    ..Default::default()
                },
            );
            for (i, line) in lines.iter().enumerate() {
                frame.fill_text(canvas::Text {
                    content: line.clone(),
                    position: Point::new(chip.x + pad, chip.y + pad + i as f32 * line_h),
                    color: if self.dark {
                        iced::Color::from_rgb(0.92, 0.92, 0.94)
                    } else {
                        iced::Color::from_rgb(0.1, 0.1, 0.12)
                    },
                    size: iced::Pixels::from(13.0),
                    ..Default::default()
                });
            }
        }
        vec![frame.into_geometry()]
    }
}

/// 按字符数截断（超长加省略号；幽灵条单行放不下主题全文）
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str) -> DropTarget {
        DropTarget {
            acct: 0,
            path: path.into(),
            label: path.into(),
        }
    }

    #[test]
    fn threshold_distinguishes_click_and_drag() {
        let p = Point::new(100.0, 100.0);
        assert!(!beyond_drag_threshold(p, Point::new(104.0, 103.0)));
        assert!(beyond_drag_threshold(p, Point::new(107.0, 100.0)));
        assert!(beyond_drag_threshold(p, Point::new(100.0, 108.0)));
    }

    #[test]
    fn target_hit_test_and_acct_filter() {
        let rects = vec![(
            Rectangle::new(Point::new(0.0, 0.0), Size::new(200.0, 28.0)),
            target("Archive"),
        )];
        assert_eq!(
            target_at(&rects, Point::new(5.0, 5.0)).unwrap().path,
            "Archive"
        );
        // 另一账号的目标仍会命中，账号过滤由调用方决定（悬停高亮同规则）
        let other = vec![(
            Rectangle::new(Point::new(0.0, 0.0), Size::new(200.0, 28.0)),
            DropTarget {
                acct: 1,
                path: "INBOX".into(),
                label: "INBOX".into(),
            },
        )];
        let hit = target_at(&other, Point::new(5.0, 5.0)).unwrap();
        assert_ne!(hit.acct, 0);
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate_chars("你好", 10), "你好");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        let long = "🦀".repeat(40);
        assert_eq!(truncate_chars(&long, 32).chars().count(), 33);
    }
}
