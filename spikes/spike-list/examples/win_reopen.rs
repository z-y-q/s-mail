//! spike：iced 0.14 daemon 零窗口后 window::open 是否还能出窗。
//!
//! 流程：启动开窗 A → 3 秒后自动关 A（零窗口）→ 再 3 秒后自动开窗 B。
//! 观察点（stdout）：
//!   - A 关闭后进程是否存活（daemon 语义）
//!   - B 的 open task 是否完成（= 窗口创建成功）
//!   - view() 为 B 渲染（= 窗口真的在画）
//! 运行：cargo run -q -p spike-list --example win_reopen

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use iced::widget::{column, container, text};
use iced::{Element, Length};

static B_RENDER_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct State {
    /// A 创建时刻（None=还没创建）
    opened_a: Option<Instant>,
    /// A 关闭时刻（None=还在）
    closed_a: Option<Instant>,
    win_a: Option<iced::window::Id>,
    win_b: bool,
}

#[derive(Debug, Clone)]
enum Msg {
    AOpened(iced::window::Id),
    BOpened(iced::window::Id),
    CloseA,
    OpenB,
    Tick,
}

fn main() -> iced::Result {
    iced::daemon(boot, update, view)
        .subscription(subscription)
        .title(|_state: &State, _window| String::from("spike-win-reopen"))
        .run()
}

fn boot() -> (State, iced::Task<Msg>) {
    let mut state = State::default();
    let (id, task) = iced::window::open(iced::window::Settings {
        size: iced::Size::new(480.0, 320.0),
        ..Default::default()
    });
    state.opened_a = Some(Instant::now());
    state.win_a = Some(id);
    (state, task.map(Msg::AOpened))
}

fn subscription(_state: &State) -> iced::Subscription<Msg> {
    iced::time::every(Duration::from_millis(200)).map(|_| Msg::Tick)
}

fn update(state: &mut State, msg: Msg) -> iced::Task<Msg> {
    match msg {
        Msg::Tick => {
            let now = Instant::now();
            // A 创建 3 秒后：关闭（进入零窗口）
            if let Some(t) = state.opened_a {
                if state.closed_a.is_none() && now - t >= Duration::from_secs(3) {
                    println!("[spike] 关闭 A（进入零窗口）");
                    state.closed_a = Some(now);
                    let Some(id) = state.win_a else {
                        return iced::Task::none();
                    };
                    state.win_a = None;
                    return iced::window::close(id);
                }
            }
            // A 关闭 3 秒后：零窗口里重开新窗（核心验证）
            if let Some(t) = state.closed_a {
                if !state.win_b && now - t >= Duration::from_secs(3) {
                    println!("[spike] 零窗口 3 秒：开窗 B…");
                    state.win_b = true;
                    let (id, task) = iced::window::open(iced::window::Settings {
                        size: iced::Size::new(480.0, 320.0),
                        ..Default::default()
                    });
                    return task.map(Msg::BOpened).chain({
                        let _ = id;
                        iced::Task::none()
                    });
                }
            }
            iced::Task::none()
        }
        Msg::AOpened(id) => {
            println!("[spike] A 已创建，3 秒后自动关闭");
            state.opened_a = Some(Instant::now());
            let _ = id;
            iced::Task::none()
        }
        Msg::BOpened(id) => {
            println!("[spike] B 已创建 {id:?}（open task 完成 = 窗口创建成功）");
            iced::Task::none()
        }
        Msg::CloseA | Msg::OpenB => iced::Task::none(),
    }
}

fn view(_state: &State, _window: iced::window::Id) -> Element<'_, Msg> {
    if _state.win_b && !B_RENDER_LOGGED.swap(true, Ordering::Relaxed) {
        println!("[spike] B 视图渲染（窗口在画 = 可见）");
    }
    container(
        column![
            text("spike：此窗 3 秒后自动关闭，再 3 秒后自动开新窗").size(14),
            text("观察终端输出").size(11),
        ]
        .spacing(8),
    )
    .padding(20)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
