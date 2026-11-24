//! Interactive terminal UI.
//!
//! The UI talks to the daemon only through the Unix socket. The logic lives
//! in [`app`] without any I/O; this module owns the terminal, the connection
//! and the event loop: it turns terminal input, timer ticks, daemon answers
//! and daemon events into [`Msg`]s and executes the [`Effect`]s the app
//! returns.

mod app;
mod diff;
mod draft;
#[cfg(test)]
mod fixtures;
mod form;
mod format;
mod help;
mod input;
mod log;
mod macs;
mod theme;
mod ui;
mod wizard;

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use futures_util::StreamExt as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use wanderlan_core::api::{self, Request};

use crate::client::{Client, ClientError};
use app::{App, Effect, Msg, Reply};

/// Options of the TUI.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// Enable mouse support. Off by default so that the terminal's own text
    /// selection keeps working.
    pub mouse: bool,
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Runs the TUI until the user quits.
pub async fn run(socket: &Path, options: TuiOptions) -> Result<()> {
    let mut terminal = setup(options.mouse).context("cannot initialise the terminal")?;
    let result = event_loop(&mut terminal, socket, options.mouse).await;
    restore(options.mouse);
    result
}

/// Raw mode, alternate screen, optional mouse capture, and a panic hook
/// that restores the terminal before the panic message is printed.
fn setup(mouse: bool) -> io::Result<Term> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(mouse);
        previous(info);
    }));
    enable_raw_mode()?;
    let terminal = (|| {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        if mouse {
            execute!(stdout, EnableMouseCapture)?;
        }
        Terminal::new(CrosstermBackend::new(stdout))
    })();
    if terminal.is_err() {
        restore(mouse);
    }
    terminal
}

/// Undoes [`setup`]; errors are ignored because there is nothing left to do.
fn restore(mouse: bool) {
    let mut stdout = io::stdout();
    if mouse {
        let _ = execute!(stdout, DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
}

/// An open connection. The generation tells answers of an old connection
/// apart after a reconnect.
#[derive(Debug)]
struct Connection {
    client: Client,
    events: mpsc::UnboundedReceiver<api::Event>,
    generation: u64,
}

/// What woke the event loop.
enum Wake {
    Terminal(Option<io::Result<TermEvent>>),
    Answer(u64, Msg),
    DaemonEvent(Option<api::Event>),
    Tick,
}

async fn next_daemon_event(conn: &mut Option<Connection>) -> Option<api::Event> {
    match conn {
        Some(c) => c.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn event_loop(terminal: &mut Term, socket: &Path, mouse: bool) -> Result<()> {
    let mut app = App::new(socket.display().to_string(), mouse);
    let (answers_tx, mut answers) = mpsc::unbounded_channel::<(u64, Msg)>();
    let mut conn: Option<Connection> = None;
    let mut generation = 0u64;
    let mut input = EventStream::new();
    let second = Duration::from_secs(1);
    let mut tick = tokio::time::interval_at(Instant::now() + second, second);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut effects: VecDeque<Effect> = app.start().into();

    loop {
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Quit => return Ok(()),
                Effect::Disconnect => conn = None,
                Effect::Connect => {
                    generation += 1;
                    let msg = match Client::connect(socket).await {
                        Ok((client, events)) => {
                            conn = Some(Connection {
                                client,
                                events,
                                generation,
                            });
                            Msg::Connected
                        }
                        Err(e) => {
                            conn = None;
                            Msg::ConnectFailed(e)
                        }
                    };
                    effects.extend(app.update(msg));
                }
                Effect::Request(request) => {
                    if let Some(c) = &conn {
                        let client = c.client.clone();
                        let tx = answers_tx.clone();
                        let generation = c.generation;
                        tokio::spawn(async move {
                            let result = perform(&client, request.clone()).await;
                            let _ = tx.send((generation, Msg::Response { request, result }));
                        });
                    }
                }
            }
        }

        terminal
            .draw(|frame| ui::draw(frame, &app))
            .context("cannot draw the screen")?;

        let wake = tokio::select! {
            ev = input.next() => Wake::Terminal(ev),
            Some((generation, msg)) = answers.recv() => Wake::Answer(generation, msg),
            ev = next_daemon_event(&mut conn) => Wake::DaemonEvent(ev),
            _ = tick.tick() => Wake::Tick,
        };
        let msg = match wake {
            Wake::Terminal(None) => return Ok(()),
            Wake::Terminal(Some(Err(e))) => {
                return Err(e).context("cannot read terminal input");
            }
            Wake::Terminal(Some(Ok(TermEvent::Key(key)))) => Msg::Key(key),
            Wake::Terminal(Some(Ok(TermEvent::Mouse(m)))) if mouse => Msg::Mouse(m),
            // Resize and other events only need a redraw.
            Wake::Terminal(Some(Ok(_))) => continue,
            Wake::Answer(g, msg) if conn.as_ref().is_some_and(|c| c.generation == g) => msg,
            Wake::Answer(..) => continue,
            Wake::DaemonEvent(Some(event)) => Msg::Event(event),
            Wake::DaemonEvent(None) => {
                conn = None;
                Msg::Disconnected(ClientError::Closed)
            }
            Wake::Tick => Msg::Tick,
        };
        effects.extend(app.update(msg));
    }
}

/// Sends a request and decodes the answer into the matching [`Reply`].
async fn perform(client: &Client, request: Request) -> Result<Reply, ClientError> {
    Ok(match request {
        r @ Request::Status => Reply::Status(Box::new(client.call(r).await?)),
        r @ Request::Subscribe => Reply::History(client.call(r).await?),
        r @ Request::GetConfig => Reply::Config(client.call(r).await?),
        r @ (Request::Apply(_) | Request::ApplyFiles(_)) => Reply::Applied(client.call(r).await?),
        r @ Request::Confirm => Reply::Confirmed(client.call(r).await?),
        r @ Request::Interfaces => Reply::Interfaces(client.call(r).await?),
        r @ Request::MacTable => Reply::Macs(client.call(r).await?),
        r @ Request::Validate(_) => {
            let _: api::Validation = client.call(r).await?;
            Reply::Done
        }
        r @ (Request::Rollback | Request::Unblock(_) | Request::ProbeNow) => {
            let _: api::Done = client.call(r).await?;
            Reply::Done
        }
    })
}
