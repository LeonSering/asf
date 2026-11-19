mod config;
mod ignore;
mod index;
mod live;
mod mover;
mod scanner;
mod ui;

use crossterm::event;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Write};
use std::time::Duration;

use scanner::scan_sessions;
use ui::{App, OpenMode, draw};

fn cmd_file_path() -> std::path::PathBuf {
    let pid = std::env::var("ASF_PARENT_PID").unwrap_or_else(|_| std::process::id().to_string());
    std::env::temp_dir().join(format!("asf-cmd-{pid}"))
}

fn main() -> io::Result<()> {
    // Ignored sessions are hidden (and not indexed) until toggled with Alt-h.
    let ignore = ignore::IgnoreList::load();
    let sessions = scan_sessions();
    let ignored: std::collections::HashSet<String> = sessions
        .iter()
        .filter(|s| !ignore.is_empty() && ignore.is_ignored(s))
        .map(|s| s.session_id.clone())
        .collect();

    enable_raw_mode()?;
    // Full-screen on the alternate screen (via stderr), so the TUI uses the
    // whole terminal and the previous terminal content is restored on exit.
    execute!(io::stderr(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;

    let config = config::Config::load();
    let mut app = App::new(sessions, ignored);
    app.open_label = config.open_label.clone();
    app.alt_label = config.alt_open.as_ref().map(|_| config.alt_label.clone());

    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;

        if event::poll(Duration::from_millis(100))? {
            let ev = event::read()?;
            app.handle_event(ev);
        }
        app.tick();

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let opened = app.open_session.and_then(|(session, mode)| {
        let cmd = config.command(mode == OpenMode::Alt, &session.session_id, &session.cwd)?;
        Some((session.cwd, cmd))
    });

    // With the shell wrapper (see shell/asf.sh), hand the folder and command
    // to the calling shell, so it changes directory and runs the command.
    if std::env::var_os("ASF_PARENT_PID").is_some() {
        let cmd_path = cmd_file_path();
        match opened {
            Some((cwd, cmd)) => {
                let mut f = std::fs::File::create(&cmd_path)?;
                writeln!(f, "{cwd}")?;
                writeln!(f, "{cmd}")?;
            }
            None => {
                let _ = std::fs::remove_file(&cmd_path);
            }
        }
        return Ok(());
    }

    // Without the wrapper: run the command directly in the session folder.
    if let Some((cwd, cmd)) = opened {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&cwd)
            .exec();
        eprintln!("asf: could not run `{cmd}` in {cwd}: {err}");
        std::process::exit(1);
    }

    Ok(())
}
