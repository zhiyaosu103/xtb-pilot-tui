//! xTB-Pilot 终端界面（仅 TUI 相关依赖在本 crate，规划文档 §2.2）。
//!
//! 当前为最小可用骨架：文本输入框（tui-input）、状态行、q/Esc 退出。

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::time::Duration;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

#[derive(Parser, Debug)]
#[command(name = "xtbp-tui", version, about = "xTB-Pilot 终端界面")]
struct Args {
    /// 组件登记表路径（InstanceRegistry）
    #[arg(long, default_value = "~/.local/share/xtbpilot/registry.toml")]
    registry: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter("xtbp_tui=info")
        .init();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, args);
    ratatui::restore();
    result
}

struct App {
    input: Input,
    registry: String,
    quit: bool,
}

fn run(terminal: &mut DefaultTerminal, args: Args) -> Result<()> {
    let mut app = App {
        input: Input::default(),
        registry: xtbp_core::config::expand_tilde(&args.registry),
        quit: false,
    };
    while !app.quit {
        terminal.draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key);
                }
            }
        }
    }
    Ok(())
}

impl App {
    fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.quit = true,
            _ => {
                // tui-input 内部处理编辑键（输入框），其余忽略
                self.input.handle_event(&Event::Key(key));
            }
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new("xTB-Pilot — q/Esc 退出 · 下方为输入框"),
        chunks[0],
    );

    let input_widget = Paragraph::new(app.input.value())
        .block(Block::bordered().title(format!("registry: {}", app.registry)));
    frame.render_widget(input_widget, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_registry_flag() {
        let args = Args::try_parse_from(["xtbp-tui", "--registry", "~/r.toml"]).unwrap();
        assert_eq!(args.registry, "~/r.toml");
    }

    #[test]
    fn esc_sets_quit() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App {
            input: Input::default(),
            registry: String::new(),
            quit: false,
        };
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.quit);
    }
}
