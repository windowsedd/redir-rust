use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, queue};
use redir_rust::{config_manager, service_ctl, status};

use crate::setup;

const ITEMS: [&str; 8] = [
    "▶ Start",
    "⏹ Stop",
    "➕ Setup Config",
    "📝 Edit Config",
    "📋 Status",
    "📊 Monitor",
    "🌐 Open GUI",
    "🚪 Exit",
];

const TITLE: &str = "╭────────────────────────────────────────╮\n│              redir-rust                │\n│        TCP / UDP redirect manager      │\n├────────────────────────────────────────┤";
const BOTTOM: &str = "╰────────────────────────────────────────╯";

pub fn run() -> ExitCode {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    loop {
        let choice = if interactive {
            select_with_arrows(TITLE, &ITEMS)
        } else {
            select_with_number()
        };
        let choice = match choice {
            Ok(Some(choice)) => choice,
            Ok(None) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: failed to read menu choice: {err}");
                return ExitCode::FAILURE;
            }
        };
        match choice {
            0 => {
                service_ctl::run("start", "redir-rust.service");
            }
            1 => {
                service_ctl::run("stop", "redir-rust.service");
            }
            2 => match config_path() {
                Ok(path) => {
                    if let Err(err) = setup::run(&path) {
                        eprintln!("error: setup failed: {err}");
                    }
                }
                Err(err) => eprintln!("error: {err}"),
            },
            3 => match config_path() {
                Ok(path) => {
                    if let Err(err) = setup::edit(&path) {
                        eprintln!("error: edit failed: {err}");
                    }
                }
                Err(err) => eprintln!("error: {err}"),
            },
            4 => {
                status::run("redir-rust.service");
            }
            5 => {
                return redir_rust::monitor::run();
            }
            6 => {
                return match config_path() {
                    Ok(path) => crate::gui::run(&path, "0.0.0.0:0".parse().unwrap()),
                    Err(err) => {
                        eprintln!("error: {err}");
                        ExitCode::FAILURE
                    }
                };
            }
            _ => return ExitCode::SUCCESS,
        }
        if interactive {
            print!("\nPress Enter to return to the menu...");
            let _ = io::stdout().flush();
            let mut pause = String::new();
            let _ = io::stdin().read_line(&mut pause);
        }
    }
}

/// Numbered prompt used when stdin/stdout is not a terminal.
fn select_with_number() -> io::Result<Option<usize>> {
    loop {
        println!("\n{TITLE}");
        for (i, item) in ITEMS.iter().enumerate() {
            println!("│  {}  {:<34}│", i + 1, item);
        }
        println!("{BOTTOM}");
        print!("Select [1-8]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 {
            return Ok(None);
        }
        match answer.trim().parse::<usize>() {
            Ok(n) if (1..=ITEMS.len()).contains(&n) => return Ok(Some(n - 1)),
            _ => eprintln!("Choose a number from 1 to 8."),
        }
    }
}

struct RawGuard;

impl RawGuard {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn draw(title: &str, items: &[&str], selected: usize, first: bool) -> io::Result<()> {
    let mut out = io::stdout();
    let height = items.len() as u16 + title.lines().count() as u16 + 2;
    if !first {
        queue!(out, cursor::MoveUp(height))?;
    }
    queue!(out, terminal::Clear(ClearType::FromCursorDown))?;
    for line in title.split('\n') {
        write!(out, "{line}\r\n")?;
    }
    for (i, item) in items.iter().enumerate() {
        let mut label = String::new();
        let mut width = 0;
        for ch in item.chars() {
            let next = ratatui::text::Span::raw(ch.to_string()).width();
            if width + next > 35 {
                break;
            }
            label.push(ch);
            width += next;
        }
        label.push_str(&" ".repeat(35 - width));
        if i == selected {
            write!(out, "│ \x1b[7m ▶ {label}\x1b[0m│\r\n")?;
        } else {
            write!(out, "│    {label}│\r\n")?;
        }
    }
    write!(out, "{BOTTOM}\r\n")?;
    write!(
        out,
        "↑/↓ move · Enter select · number keys jump · Esc previous\r\n"
    )?;
    out.flush()
}

/// Arrow-key menu. Returns `None` when the user quits (q / Esc / Ctrl-C).
pub(crate) fn select_with_arrows(title: &str, items: &[&str]) -> io::Result<Option<usize>> {
    let mut selected = 0usize;
    println!();
    let _raw = RawGuard::new()?;
    draw(title, items, selected, true)?;
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = (selected + items.len() - 1) % items.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                selected = (selected + 1) % items.len();
            }
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = items.len() - 1,
            KeyCode::Enter => return Ok(Some(selected)),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Char('0') => return Ok(None),
            KeyCode::Char(c) => {
                if let Some(n) = c.to_digit(10) {
                    if (1..=items.len() as u32).contains(&n) {
                        return Ok(Some(n as usize - 1));
                    }
                }
            }
            _ => continue,
        }
        draw(title, items, selected, false)?;
    }
}

fn config_path() -> std::io::Result<std::path::PathBuf> {
    config_manager::configured_path(&config_manager::settings_path())
}
