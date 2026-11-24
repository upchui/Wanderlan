use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wanderlan_core::api::{ChangeSource, Event, Level, PendingChange, Request, SiteState};
use wanderlan_core::config::VlanSet;

use super::*;
use crate::client::ClientError;
use crate::tui::fixtures;

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> Msg {
    key(KeyCode::Char(c))
}

fn reply(request: Request, reply: Reply) -> Msg {
    Msg::Response {
        request,
        result: Ok(reply),
    }
}

/// An app connected to a daemon that reports the fixture status and config.
fn connected() -> App {
    let mut app = App::new("/run/wanderlan/wanderlan.sock", false);
    app.start();
    app.update(Msg::Connected);
    app.update(reply(
        Request::Status,
        Reply::Status(Box::new(fixtures::status())),
    ));
    app.update(reply(
        Request::GetConfig,
        Reply::Config(fixtures::config_texts()),
    ));
    app
}

fn with_pending(app: &mut App, source: ChangeSource) {
    let mut status = fixtures::status();
    status.pending = Some(PendingChange {
        remaining_secs: 42,
        source,
    });
    app.update(reply(Request::Status, Reply::Status(Box::new(status))));
}

fn requests(fx: &[Effect]) -> Vec<&Request> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Request(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[test]
fn connects_and_retries_when_the_daemon_is_down() {
    let mut app = App::new("/run/wanderlan/wanderlan.sock", false);
    assert_eq!(app.start(), vec![Effect::Connect]);
    let fx = app.update(Msg::ConnectFailed(ClientError::NotRunning {
        path: PathBuf::from("/run/wanderlan/wanderlan.sock"),
    }));
    assert!(fx.is_empty());
    match &app.conn {
        Conn::Down { error, .. } => assert!(error.contains("daemon is not running")),
        other => panic!("unexpected {other:?}"),
    }
    let mut reconnect = Vec::new();
    for _ in 0..RETRY_TICKS {
        reconnect.extend(app.update(Msg::Tick));
    }
    assert_eq!(reconnect, vec![Effect::Connect]);
    assert_eq!(app.conn, Conn::Connecting);
}

#[test]
fn connecting_requests_status_config_and_events() {
    let mut app = App::new("/s", false);
    app.start();
    let fx = app.update(Msg::Connected);
    let reqs = requests(&fx);
    assert!(reqs.contains(&&Request::Status));
    assert!(reqs.contains(&&Request::GetConfig));
    assert!(reqs.contains(&&Request::Subscribe));
    assert!(app.is_connected());
}

#[test]
fn unconfigured_site_opens_the_wizard() {
    let mut app = App::new("/s", false);
    app.start();
    app.update(Msg::Connected);
    let fx = app.update(reply(
        Request::Status,
        Reply::Status(Box::new(fixtures::unconfigured_status())),
    ));
    assert_eq!(app.view, View::Setup);
    assert!(requests(&fx).contains(&&Request::Interfaces));
    assert_eq!(
        app.status.as_ref().map(|s| s.state),
        Some(SiteState::Unconfigured)
    );
}

#[test]
fn configured_site_starts_on_the_overview() {
    let app = connected();
    assert_eq!(app.view, View::Overview);
    assert!(app.draft.is_some());
    assert!(!app.draft_dirty());
}

#[test]
fn digits_tab_and_help_switch_views() {
    let mut app = connected();
    app.update(ch('2'));
    assert_eq!(app.view, View::Sites);
    app.update(key(KeyCode::Tab));
    assert_eq!(app.view, View::Vlans);
    let fx = app.update(ch('4'));
    assert_eq!(app.view, View::Macs);
    assert!(requests(&fx).contains(&&Request::MacTable));
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.view, View::Vlans);
    app.update(ch('?'));
    assert!(matches!(app.modal, Some(Modal::Help { .. })));
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
}

#[test]
fn ctrl_c_quits_immediately() {
    let mut app = connected();
    let fx = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(fx.contains(&Effect::Quit));
}

#[test]
fn quit_asks_first_when_the_draft_has_changes() {
    let mut app = connected();
    assert!(app.update(ch('q')).contains(&Effect::Quit));

    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.mesh.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
    }
    assert!(app.draft_dirty());
    let fx = app.update(ch('q'));
    assert!(!fx.contains(&Effect::Quit));
    assert!(matches!(app.modal, Some(Modal::Ask(_))));
    assert!(app.update(ch('y')).contains(&Effect::Quit));
}

#[test]
fn review_shows_the_diff_and_applies_with_the_base_revision() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.mesh.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
    }
    app.update(ch('a'));
    let Some(Modal::Review(review)) = &app.modal else {
        panic!("review expected, got {:?}", app.modal);
    };
    assert_eq!(review.state, ReviewState::Ready);
    assert!(review.is_valid());
    assert!(!review.lines.is_empty());
    let fx = app.update(key(KeyCode::Enter));
    let apply = requests(&fx)
        .into_iter()
        .find_map(|r| match r {
            Request::Apply(p) => Some(p.clone()),
            _ => None,
        })
        .expect("apply request");
    assert_eq!(apply.base_revision, Some(7));
    assert!(apply.config.mesh.contains("vlans = \"10-60\""));
    assert!(apply.config.local.contains("self = \"heim\""));
    assert_eq!(app.busy, Some(Busy::Applying));
}

#[test]
fn invalid_draft_is_not_applied() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.mesh.sites[2].hub = Some("nowhere".into());
        d.revalidate();
        assert!(d.errors() > 0);
    }
    app.update(ch('a'));
    let fx = app.update(key(KeyCode::Enter));
    assert!(!requests(&fx).iter().any(|r| matches!(r, Request::Apply(_))));
}

#[test]
fn pending_change_can_be_confirmed_or_rolled_back() {
    let mut app = connected();
    with_pending(&mut app, ChangeSource::Local);
    assert_eq!(app.pending().map(|p| p.remaining_secs), Some(42));
    let fx = app.update(ch('c'));
    assert_eq!(requests(&fx), vec![&Request::Confirm]);
    assert_eq!(app.busy, Some(Busy::Confirming));

    let mut app = connected();
    with_pending(&mut app, ChangeSource::Local);
    let fx = app.update(ch('r'));
    assert_eq!(requests(&fx), vec![&Request::Rollback]);
}

#[test]
fn confirm_without_pending_change_only_informs() {
    let mut app = connected();
    let fx = app.update(ch('c'));
    assert!(requests(&fx).is_empty());
    assert!(app.message.is_some());
}

#[test]
fn banner_names_the_source_and_the_remaining_time() {
    let local = PendingChange {
        remaining_secs: 9,
        source: ChangeSource::Local,
    };
    let text = banner_text(&local);
    assert!(text.contains("UNCONFIRMED CHANGE: rollback in 9 s"));
    assert!(text.contains("c confirm"));
    let synced = PendingChange {
        remaining_secs: 30,
        source: ChangeSource::Sync {
            peer: "buero".into(),
        },
    };
    assert!(banner_text(&synced).contains("CHANGE FROM buero"));
}

#[test]
fn live_events_reach_the_log() {
    let mut app = connected();
    app.update(reply(
        Request::Subscribe,
        Reply::History(fixtures::events()),
    ));
    let before = app.log.events.len();
    app.update(Msg::Event(Event {
        time_ms: fixtures::T0,
        level: Level::Warn,
        peer: Some("haus2".into()),
        message: "Loop detected".into(),
    }));
    assert_eq!(app.log.events.len(), before + 1);
}

#[test]
fn lost_connection_while_applying_marks_the_review_failed() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.mesh.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
    }
    app.update(ch('a'));
    app.update(key(KeyCode::Enter));
    app.update(Msg::Disconnected(ClientError::Closed));
    let Some(Modal::Review(review)) = &app.modal else {
        panic!("review expected");
    };
    assert!(matches!(review.state, ReviewState::Failed(_)));
    assert!(matches!(app.conn, Conn::Down { .. }));
}
