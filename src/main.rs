use std::time::Duration;

use hdir::app::App;
use hdir::term;
use ratatui::crossterm::event;

fn main() -> anyhow::Result<()> {
    term::install_panic_hook();
    let mut terminal = term::init()?;
    let mut app = App::default();

    let result = run(&mut terminal, &mut app);
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
            dirty = app.handle_event(event::read()?);
        }
    }
    Ok(())
}
