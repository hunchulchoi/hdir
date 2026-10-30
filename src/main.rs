use std::time::Duration;

use std::path::PathBuf;
use std::process::ExitCode;

use hdir::app::App;
use hdir::cli::{self, Cli};
use hdir::config;
use hdir::term;
use ratatui::crossterm::event;

fn main() -> ExitCode {
    let opts = match cli::parse_args(std::env::args_os().skip(1)) {
        Ok(Cli::Run(o)) => o,
        Ok(Cli::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Cli::Version) => {
            println!("{} {}", hdir::APP_NAME, hdir::VERSION);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("hdir: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let start = match start_dir(opts.start_dir.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hdir: {e}");
            return ExitCode::from(2);
        }
    };
    let mut cwd_file = opts.cwd_file;
    if cfg!(windows) && cwd_file.take().is_some() {
        eprintln!("hdir: --cwd-file: Windows 지원 예정 (무시함)");
    }

    let mut app = App::new(start.clone(), start);
    let result = run_app(&mut app);
    // The terminal is back to normal here, so errors are visible.
    let mut code = ExitCode::SUCCESS;
    if let Err(e) = result {
        eprintln!("hdir: {e}");
        code = ExitCode::FAILURE;
    }
    if let Some(path) = cwd_file
        && let Err(e) = cli::write_cwd_file(&path, app.exit_cwd())
    {
        eprintln!("hdir: --cwd-file {}: {e}", path.display());
        code = ExitCode::FAILURE;
    }
    code
}

fn start_dir(arg: Option<&std::path::Path>) -> Result<PathBuf, String> {
    let Some(arg) = arg else {
        return Ok(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    };
    let abs = std::path::absolute(arg).map_err(|e| format!("{}: {e}", arg.display()))?;
    if !abs.is_dir() {
        return Err(format!("{}: 디렉터리가 아님", arg.display()));
    }
    Ok(hdir::cmdline::normalize(&abs))
}

fn run_app(app: &mut App) -> anyhow::Result<()> {
    term::install_panic_hook();
    let mut terminal = term::init()?;
    app.configure(
        config::default_config_dir(),
        config::colorterm_value(std::env::var("COLORTERM").ok(), cfg!(windows)).as_deref(),
    );

    let result = run(&mut terminal, app);
    term::restore()?;
    result
}

fn run(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    let mut dirty = true;
    while app.running {
        if dirty {
            terminal.draw(|f| app.draw(f))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(50))? {
            dirty |= app.handle_event(event::read()?);
        }
        if app.has_pending_edit() {
            app.run_edit(|f| term::suspend(terminal, f));
            dirty = true;
        }
        if app.has_pending_shell() {
            app.run_shell(|f| term::suspend(terminal, f));
            dirty = true;
        }
        if app.has_pending_run() {
            app.run_pending(|f| term::suspend(terminal, f));
            dirty = true;
        }
        dirty |= app.tick();
    }
    Ok(())
}
