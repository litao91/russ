#![forbid(unsafe_code)]

use crate::modes::{Mode, Selected};
use anyhow::Result;
use app::App;
use clap::{Parser, Subcommand};
use crossterm::event::{self, KeyEvent, KeyEventKind};
use crossterm::event::{Event as CEvent, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::stdout;
use std::path::PathBuf;
use std::time;
use tokio::sync::mpsc;

mod app;
mod io;
mod modes;
mod opml;
mod rss;
mod ui;
mod util;

fn main() -> Result<()> {
    let options = Options::parse();

    let validated_options = options.subcommand.validate()?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match validated_options {
        ValidatedOptions::Import(options) => {
            rt.block_on(async move { crate::opml::import(options).await })
        }
        ValidatedOptions::Read(options) => rt.block_on(async move { run_reader(options).await }),
    }
}

/// A TUI RSS reader with vim-like controls and a local-first, offline-first focus
#[derive(Debug, Parser)]
#[command(author, version, about, name = "russ")]
struct Options {
    #[command(subcommand)]
    subcommand: Command,
}

/// Only used to take input at the boundary.
/// Turned into `ValidatedOptions` with `validate()`.
#[derive(Debug, Subcommand)]
enum Command {
    /// Read your feeds
    Read {
        /// Override where `russ` stores and reads feeds.
        /// By default, the feeds database on Linux this will be at `XDG_DATA_HOME/russ/feeds.db` or `$HOME/.local/share/russ/feeds.db`.
        /// On MacOS it will be at `$HOME/Library/Application Support/russ/feeds.db`.
        /// On Windows it will be at `{FOLDERID_LocalAppData}/russ/data/feeds.db`.
        #[arg(short, long)]
        database_path: Option<PathBuf>,
        /// time in ms between two ticks
        #[arg(short, long, default_value = "250")]
        tick_rate: u64,
        /// number of seconds to show the flash message before clearing it
        #[arg(short, long, default_value = "4", value_parser = parse_seconds)]
        flash_display_duration_seconds: time::Duration,
        /// RSS/Atom network request timeout in seconds
        #[arg(short, long, default_value = "5", value_parser = parse_seconds)]
        network_timeout: time::Duration,
    },
    /// Import feeds from an OPML document
    Import {
        /// Override where `russ` stores and reads feeds.
        /// By default, the feeds database on Linux this will be at `XDG_DATA_HOME/russ/feeds.db` or `$HOME/.local/share/russ/feeds.db`.
        /// On MacOS it will be at `$HOME/Library/Application Support/russ/feeds.db`.
        /// On Windows it will be at `{FOLDERID_LocalAppData}/russ/data/feeds.db`.
        #[arg(short, long)]
        database_path: Option<PathBuf>,
        #[arg(short, long)]
        opml_path: PathBuf,
        /// RSS/Atom network request timeout in seconds
        #[arg(short, long, default_value = "5", value_parser = parse_seconds)]
        network_timeout: time::Duration,
    },
}

impl Command {
    fn validate(&self) -> std::io::Result<ValidatedOptions> {
        match self {
            Command::Read {
                database_path,
                tick_rate,
                flash_display_duration_seconds,
                network_timeout,
            } => {
                let database_path = get_database_path(database_path)?;

                Ok(ValidatedOptions::Read(ReadOptions {
                    database_path,
                    tick_rate: *tick_rate,
                    flash_display_duration_seconds: *flash_display_duration_seconds,
                    network_timeout: *network_timeout,
                }))
            }
            Command::Import {
                database_path,
                opml_path,
                network_timeout,
            } => {
                let database_path = get_database_path(database_path)?;
                Ok(ValidatedOptions::Import(ImportOptions {
                    database_path,
                    opml_path: opml_path.to_owned(),
                    network_timeout: *network_timeout,
                }))
            }
        }
    }
}

fn parse_seconds(s: &str) -> Result<time::Duration, std::num::ParseIntError> {
    let as_u64 = s.parse::<u64>()?;
    Ok(time::Duration::from_secs(as_u64))
}

/// internal, validated options for the normal reader mode
#[derive(Debug)]
enum ValidatedOptions {
    Read(ReadOptions),
    Import(ImportOptions),
}

#[derive(Clone, Debug)]
struct ReadOptions {
    database_path: PathBuf,
    tick_rate: u64,
    flash_display_duration_seconds: time::Duration,
    network_timeout: time::Duration,
}

#[derive(Debug)]
struct ImportOptions {
    database_path: PathBuf,
    opml_path: PathBuf,
    network_timeout: time::Duration,
}

fn get_database_path(database_path: &Option<PathBuf>) -> std::io::Result<PathBuf> {
    let database_path = if let Some(database_path) = database_path {
        database_path.to_owned()
    } else {
        let mut database_path = directories::ProjectDirs::from("", "", "russ")
            .expect("unable to find home directory. if you like, you can provide a database path directly by passing the -d option.")
            .data_local_dir()
            .to_path_buf();

        std::fs::create_dir_all(&database_path)?;

        database_path.push("feeds.db");

        database_path
    };

    Ok(database_path)
}

pub enum Event<I> {
    Input(I),
    Tick,
}

async fn run_reader(options: ReadOptions) -> Result<()> {
    enable_raw_mode()?;

    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);

    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    // Setup input handling
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    let event_tx_clone = event_tx.clone();

    let tick_rate = time::Duration::from_millis(options.tick_rate);
    let options_clone = options.clone();

    let (io_tx, mut io_rx) = mpsc::unbounded_channel();

    let io_tx_clone = io_tx.clone();

    let mut app = App::new(options, event_tx_clone, io_tx)?;

    terminal.clear()?;

    // this is basically "the Elm Architecture".
    //
    // more or less:
    // ui <- current_state
    // action <- current_state + event
    // new_state <- current_state + action
    let mut tick_interval = tokio::time::interval(tick_rate);
    loop {
        app.draw(&mut terminal).await?;
        tokio::select! {
            _tick = tick_interval.tick() => {
                event_tx.send(Event::Tick).expect("Unable to send tick");
            },
            Some(event) = event_rx.recv() => {
                let action = get_action(&app, event).await;

                if let Some(action) = action {
                        update(&mut app, action).await?;
                }

                if app.should_quit().await {
                    disable_raw_mode()?;
                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                    terminal.show_cursor()?;
                    break;
                }
            },
            Some(event) = io_rx.recv() => {
            io::io_loop(&app, &io_tx_clone, &options_clone, event).await?;
            }
            Ok(ready) = tokio::task::spawn_blocking(|| crossterm::event::poll(time::Duration::from_millis(100))) => {
                match ready {
                    Ok(true) => {
                        if let CEvent::Key(key) = event::read().expect("Unable to read Crossterm event") {
                            event_tx
                                .send(Event::Input(key))
                                .expect("Unable to send Crossterm Key input event");
                        }
                    }
                    Ok(false) => continue,
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to poll for events: {:?}", e).into());
                    }
                }
            }


        }
    }

    Ok(())
}

enum Action {
    Quit,
    MoveLeft,
    MoveDown,
    MoveUp,
    MoveRight,
    PageUp,
    PageDown,
    RefreshAll,
    RefreshFeed,
    ToggleHelp,
    ToggleReadMode,
    EnterEditingMode,
    OpenLinkInBrowser,
    CopyLinkToClipboard,
    ScrapeArticle,
    Tick,
    SubscribeToFeed,
    PushInputChar(char),
    DeleteInputChar,
    DeleteFeed,
    EnterNormalMode,
    ClearErrorFlash,
    SelectAndShowCurrentEntry,
    ToggleReadStatus,
}

async fn get_action(app: &App, event: Event<KeyEvent>) -> Option<Action> {
    match app.mode().await {
        Mode::Normal => match event {
            Event::Input(key_event) if key_event.kind == KeyEventKind::Press => {
                match (key_event.code, key_event.modifiers) {
                    (KeyCode::Char('q'), _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Esc, _) => {
                        if !app.error_flash_is_empty().await {
                            Some(Action::ClearErrorFlash)
                        } else {
                            Some(Action::Quit)
                        }
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => match app.selected().await {
                        Selected::Feeds => Some(Action::RefreshFeed),
                        _ => Some(Action::ToggleReadStatus),
                    },
                    (KeyCode::Char('x'), KeyModifiers::NONE) => Some(Action::RefreshAll),
                    (KeyCode::Left, _) | (KeyCode::Char('h'), _) => Some(Action::MoveLeft),
                    (KeyCode::Right, _) | (KeyCode::Char('l'), _) => Some(Action::MoveRight),
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => Some(Action::MoveDown),
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => Some(Action::MoveUp),
                    (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        Some(Action::PageUp)
                    }
                    (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        Some(Action::PageDown)
                    }
                    (KeyCode::Enter, _) => match app.selected().await {
                        Selected::Entries | Selected::Entry(_) => {
                            if app.has_entries().await && app.has_current_entry().await {
                                Some(Action::SelectAndShowCurrentEntry)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    },
                    (KeyCode::Char('?'), _) => Some(Action::ToggleHelp),
                    (KeyCode::Char('a'), _) => Some(Action::ToggleReadMode),
                    (KeyCode::Char('e'), _) | (KeyCode::Char('i'), _) => {
                        Some(Action::EnterEditingMode)
                    }
                    (KeyCode::Char('c'), _) => Some(Action::CopyLinkToClipboard),
                    (KeyCode::Char('o'), _) => Some(Action::OpenLinkInBrowser),
                    (KeyCode::Char('s'), _) => Some(Action::ScrapeArticle),
                    _ => None,
                }
            }
            Event::Input(_) => None,
            Event::Tick => Some(Action::Tick),
        },
        Mode::Editing => match event {
            Event::Input(key_event) if key_event.kind == KeyEventKind::Press => {
                match key_event.code {
                    KeyCode::Enter => {
                        if !app.feed_subscription_input_is_empty().await {
                            Some(Action::SubscribeToFeed)
                        } else {
                            None
                        }
                    }
                    KeyCode::Char(c) => Some(Action::PushInputChar(c)),
                    KeyCode::Backspace => Some(Action::DeleteInputChar),
                    KeyCode::Delete => Some(Action::DeleteFeed),
                    KeyCode::Esc => Some(Action::EnterNormalMode),
                    _ => None,
                }
            }
            Event::Input(_) => None,
            Event::Tick => Some(Action::Tick),
        },
    }
}

async fn update(app: &mut App, action: Action) -> Result<()> {
    match action {
        Action::Tick => (),
        Action::Quit => app.set_should_quit(true).await,
        Action::RefreshAll => app.refresh_feeds().await?,
        Action::RefreshFeed => app.refresh_feed().await?,
        Action::MoveLeft => app.on_left().await?,
        Action::MoveDown => app.on_down().await?,
        Action::MoveUp => app.on_up().await?,
        Action::MoveRight => app.on_right().await?,
        Action::PageUp => app.page_up().await,
        Action::PageDown => app.page_down().await,
        Action::ToggleHelp => app.toggle_help().await?,
        Action::ToggleReadMode => app.toggle_read_mode().await?,
        Action::ToggleReadStatus => app.toggle_read().await?,
        Action::EnterEditingMode => app.set_mode(Mode::Editing).await,
        Action::CopyLinkToClipboard => app.put_current_link_in_clipboard().await?,
        Action::OpenLinkInBrowser => app.open_link_in_browser().await?,
        Action::ScrapeArticle => app.scrape_article().await?,
        Action::SubscribeToFeed => app.subscribe_to_feed().await?,
        Action::PushInputChar(c) => app.push_feed_subscription_input(c).await,
        Action::DeleteInputChar => app.pop_feed_subscription_input().await,
        Action::DeleteFeed => app.delete_feed().await?,
        Action::EnterNormalMode => app.set_mode(Mode::Normal).await,
        Action::ClearErrorFlash => app.clear_error_flash().await,
        Action::SelectAndShowCurrentEntry => app.select_and_show_current_entry().await?,
    };

    Ok(())
}
