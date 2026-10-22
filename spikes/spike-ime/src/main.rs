//! S1 · IME spike（一票否决项，docs/analysis/round-2 §4.1）
//!
//! 运行：cargo run -p spike-ime --release
//!
//! 验收清单（macOS 拼音输入法，人工逐项打勾）：
//!   [ ] 候选框出现在光标处（不是屏幕左下角/原点）
//!   [ ] preedit（未上屏的拼音/组合字）内联显示在编辑器内
//!   [ ] 数字键选词 / 回车上屏正确
//!   [ ] 上屏后光标位置正确，继续输入正常
//!   [ ] 删除键能删 preedit / 已上屏文字
//!
//! 任一项不过 → 记录现象，换 egui 0.36.x 复测（同清单）。

use iced::widget::{column, text, text_editor};
use iced::{Element, Task};

fn main() -> iced::Result {
    // iced 0.14：首参数是 boot 函数（返回 State），标题用 builder 设置
    iced::application(State::default, update, view)
        .title("S1 · IME spike")
        .window_size((640.0, 480.0))
        .run()
}

#[derive(Default)]
struct State {
    content: text_editor::Content,
}

#[derive(Debug, Clone)]
enum Message {
    Edit(text_editor::Action),
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Edit(action) => {
            // iced 0.14：直接在 Content 上 perform
            state.content.perform(action);
            Task::none()
        }
    }
}

fn view(state: &State) -> Element<'_, Message> {
    column![
        text("用拼音输入法在下方写一段中文，逐项核对窗口顶部注释里的验收清单。"),
        text_editor(&state.content)
            .on_action(Message::Edit)
            .height(320),
        text(format!(
            "字符数：{}（上屏正常与否的辅助指标）",
            state.content.text().chars().count()
        )),
    ]
    .spacing(12)
    .padding(20)
    .into()
}
