//! S2 · 虚拟列表 spike（一票否决项，docs/analysis/round-2 §4.2）
//!
//! 运行：cargo run -p spike-list --release
//!
//! 验收：10 万行滚动无明显掉帧；顶部标签实时显示当前窗口（first..last），
//! 证明只有可见区 ± BUFFER 行在构建 widget。
//!
//! 实现：iced scrollable 无自动回收，这里用"上/下占位 Space + 只构建可见窗口"
//! 的手动窗口化 —— 这段代码验收通过后直接搬进 smail-ui 的邮件列表。

use iced::widget::{column, container, scrollable, text, Space};
use iced::{Element, Length, Task};

const ROWS: usize = 100_000;
const ROW_H: f32 = 26.0;
const VIEW_H: f32 = 520.0;
const BUFFER: usize = 30;

fn main() -> iced::Result {
    iced::application(State::default, update, view)
        .title("S2 · virtual list spike")
        .window_size((720.0, 640.0))
        .run()
}

#[derive(Default)]
struct State {
    offset_y: f32,
}

#[derive(Debug, Clone)]
enum Message {
    Scrolled(f32),
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Scrolled(y) => {
            state.offset_y = y;
            Task::none()
        }
    }
}

fn view(state: &State) -> Element<'_, Message> {
    let first = ((state.offset_y / ROW_H) as usize).saturating_sub(BUFFER);
    let visible = (VIEW_H / ROW_H).ceil() as usize + BUFFER * 2;
    let last = (first + visible).min(ROWS);

    let top_pad = first as f32 * ROW_H;
    let bottom_pad = (ROWS - last) as f32 * ROW_H;

    let mut list = column![Space::new()
        .width(Length::Fill)
        .height(Length::Fixed(top_pad)),];

    for i in first..last {
        list = list.push(
            container(text(format!(
                "邮件 #{i:06} · 某发件人 · 关于某主题的往来讨论"
            )))
            .height(ROW_H)
            .padding([3, 8]),
        );
    }

    list = list.push(
        Space::new()
            .width(Length::Fill)
            .height(Length::Fixed(bottom_pad)),
    );

    column![
        text(format!(
            "窗口: {first}..{last} / {ROWS}（offset {:.0}px）",
            state.offset_y
        )),
        scrollable(list)
            .height(VIEW_H)
            .on_scroll(|vp: scrollable::Viewport| Message::Scrolled(vp.absolute_offset().y)),
    ]
    .spacing(8)
    .padding(16)
    .into()
}
