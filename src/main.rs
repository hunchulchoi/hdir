use std::time::Duration;

use hdir::app::App;
use hdir::term;
use ratatui::crossterm::event::{self, Event};

fn main() -> anyhow::Result<()> {
    term::install_panic_hook();
    let mut terminal = term::init()?;
    let mut app = App::new();

    let result = run(&mut terminal, &mut app);
    term::restore()?;
    result
}

fn run(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    while app.running {
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
    }
    Ok(())
}
