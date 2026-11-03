use std::time::Duration;

use std::io::IsTerminal;
use std::process::ExitCode;

use hdir::app::App;
use hdir::cli::{self, Cli};
use hdir::config;
use hdir::i18n;
use hdir::term;
use ratatui::crossterm::event;

fn main() -> ExitCode {
    let lang = i18n::detect(|k| std::env::var(k).ok(), i18n::os_lang());
    i18n::set_lang(lang);
    let opts = match cli::parse_args(std::env::args_os().skip(1)) {
        Ok(Cli::Run(o)) => o,
        Ok(Cli::Help) => {
            print!("{}", cli::usage());
            return ExitCode::SUCCESS;
        }
        Ok(Cli::Version) => {
            println!("{} {}", hdir::APP_NAME, hdir::VERSION);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("hdir: {e}\n\n{}", cli::usage());
            return ExitCode::from(2);
        }
    };
    let start = match cli::start_dir(opts.start_dir.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hdir: {e}");
            return ExitCode::from(2);
        }
    };
    let cwd_file = opts.cwd_file;
    if let Some(e) = cli::no_terminal(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        eprintln!("hdir: {e}");
        return ExitCode::from(2);
    }

    let mut app = App::new(start.clone(), start);
    app.base_lang = lang;
    app.ssh_config_path = dirs::home_dir().map(|h| h.join(".ssh").join("config"));
    let result = run_app(&mut app);
    // An edited remote file may still be uploading.
    // The terminal is back to normal here, so the report is visible.
    let uploaded = app.quit_uploads(Duration::from_secs(120), &mut std::io::stderr());
    let mut code = if uploaded {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
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
        if app.has_pending_master() {
            app.run_master(|f| term::suspend(terminal, f));
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
