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
    assert_eq!(fx, vec![Effect::Diagnose], "asks systemd why");
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
    let fx = app.update(Msg::ConnectFailed(ClientError::NotRunning {
        path: PathBuf::from("/run/wanderlan/wanderlan.sock"),
    }));
    assert!(fx.is_empty(), "not on every attempt");
    app.update(Msg::Diagnosis(crate::service::Diagnosis {
        state: Some("failed".into()),
        journal: vec!["boom".into()],
    }));
    assert!(app.diagnosis.is_some());
    app.update(Msg::Connected);
    assert_eq!(app.diagnosis, None, "forgotten once connected");
}

#[test]
fn after_a_reset_the_kernel_devices_are_cleaned_up_and_everything_reloaded() {
    let mut app = App::new("/s", false);
    app.start();
    app.update(Msg::Connected);
    let fx = app.update(Msg::Response {
        request: Request::Reset,
        result: Ok(Reply::Reset(ResetReport::default())),
    });
    let reqs = requests(&fx);
    assert!(reqs.contains(&&Request::Interfaces));
    assert!(reqs.contains(&&Request::Status));
    let fx = app.update(Msg::Response {
        request: Request::Interfaces,
        result: Ok(Reply::Interfaces(vec![])),
    });
    assert_eq!(fx, vec![Effect::KernelCleanup(vec![])]);
    let fx = app.update(Msg::KernelCleaned(Some("Unloaded".into())));
    assert!(
        requests(&fx).contains(&&Request::Interfaces),
        "the list is reloaded"
    );
    app.update(Msg::Response {
        request: Request::Interfaces,
        result: Ok(Reply::Interfaces(vec![])),
    });
    // A failed reset also reloads the real state.
    let fx = app.update(Msg::Response {
        request: Request::Reset,
        result: Err(ClientError::Api(ApiError::new("boom"))),
    });
    assert!(requests(&fx).contains(&&Request::Interfaces));
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
        d.mesh.sites[2].vlans = Some(wanderlan_core::config::VlanSet::new());
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

#[test]
fn an_invitation_carries_the_rules_of_the_form() {
    let mut app = connected();
    app.update(ch('2'));
    app.update(ch('i'));
    assert!(matches!(app.modal, Some(Modal::Form(_))));
    for c in "haus7".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Tab));
    for c in "native,20,30".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    for c in "130:30".chars() {
        app.update(ch(c));
    }
    let fx = app.update(key(KeyCode::Enter));
    let sent: Vec<_> = requests(&fx)
        .into_iter()
        .filter_map(|r| match r {
            Request::Invite(rules) => Some(rules.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "{fx:?}");
    assert_eq!(sent[0].name.as_deref(), Some("haus7"));
    assert_eq!(sent[0].vlans.as_deref(), Some("native,20,30"));
    assert_eq!(sent[0].translations.len(), 1);
    assert!(!sent[0].allow_self);
    assert!(app.modal.is_none());
    assert!(
        !app.draft_dirty(),
        "an invitation does not change the draft"
    );
}

#[test]
fn invalid_invitation_rules_stay_in_the_form() {
    let mut app = connected();
    app.update(ch('2'));
    app.update(ch('i'));
    // A name that exists already.
    for c in "buero".chars() {
        app.update(ch(c));
    }
    let fx = app.update(key(KeyCode::Enter));
    assert!(
        !requests(&fx)
            .iter()
            .any(|r| matches!(r, Request::Invite(_)))
    );
    assert!(matches!(app.modal, Some(Modal::Form(_))));
}

#[test]
fn a_site_goes_online_through_the_draft() {
    let mut app = connected();
    app.update(ch('2'));
    let haus3 = app
        .draft
        .as_ref()
        .and_then(|d| d.mesh.sites.iter().position(|s| s.name == "haus3"))
        .unwrap();
    app.sites_selected = haus3;
    app.update(ch('o'));
    assert!(app.draft_dirty());
    let site = app.draft.as_ref().unwrap().mesh.site("haus3").unwrap();
    assert!(!site.online);
}

#[test]
fn a_site_managed_elsewhere_cannot_be_changed_here() {
    let mut app = connected();
    app.update(ch('2'));
    let haus6 = app
        .draft
        .as_ref()
        .and_then(|d| d.mesh.sites.iter().position(|s| s.name == "haus6"))
        .unwrap();
    app.sites_selected = haus6;
    app.update(ch('o'));
    assert!(!app.draft_dirty(), "haus6 is managed by buero");
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("managed by only \"buero\""))
    );
    app.update(ch('e'));
    assert!(app.modal.is_none());
}

/// The fixture app on a site that is managed by "buero" (restricted).
fn restricted() -> App {
    let mut app = connected();
    let mut texts = fixtures::config_texts();
    let heim = "name = \"heim\"\naddress = \"10.8.0.1\"\n";
    assert!(texts.mesh.contains(heim), "{}", texts.mesh);
    texts.mesh = texts
        .mesh
        .replace(heim, &format!("{heim}managed_by = \"buero\"\n"));
    app.update(reply(Request::GetConfig, Reply::Config(texts)));
    app
}

#[test]
fn a_restricted_site_changes_no_vlans() {
    let mut app = restricted();
    assert_eq!(app.restricted_by().as_deref(), Some("buero"));
    let refused = |app: &App| {
        app.modal.is_none()
            && app
                .message
                .as_ref()
                .is_some_and(|m| m.text.contains("\"buero\""))
    };
    // The network's VLANs, in the VLAN view and in the settings.
    app.update(ch('3'));
    app.update(ch('v'));
    assert!(refused(&app));
    app.message = None;
    app.update(ch('7'));
    app.settings_selected = 0;
    app.update(key(KeyCode::Enter));
    assert!(refused(&app));
    // An open site (here buero, which has no manager).
    app.message = None;
    app.update(ch('2'));
    app.sites_selected = 1;
    app.update(ch('e'));
    assert!(refused(&app));
    // Technical settings stay open.
    app.update(ch('7'));
    app.settings_selected = 1;
    app.update(key(KeyCode::Enter));
    assert!(matches!(app.modal, Some(Modal::Form(_))));
}

#[test]
fn after_the_setup_the_invitation_form_opens() {
    let mut app = connected();
    app.wizard.step = Step::Applied;
    let fx = app.update(reply(
        Request::Confirm,
        Reply::Confirmed(Confirmed { revision: 1 }),
    ));
    assert!(requests(&fx).contains(&&Request::GetConfig));
    assert!(
        !requests(&fx)
            .iter()
            .any(|r| matches!(r, Request::Invite(_))),
        "no invitation without its rules"
    );
    app.update(reply(
        Request::GetConfig,
        Reply::Config(fixtures::config_texts()),
    ));
    assert!(matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::Invite));
}

#[test]
fn enter_on_a_peer_opens_its_details_and_history_grows() {
    let mut app = connected();
    for _ in 0..3 {
        app.update(reply(
            Request::Status,
            Reply::Status(Box::new(fixtures::status())),
        ));
    }
    assert_eq!(app.history.get("buero").map(VecDeque::len), Some(4));
    app.update(key(KeyCode::Enter));
    assert!(matches!(&app.modal, Some(Modal::Peer { name }) if name == "buero"));
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
}

#[test]
fn settings_open_the_form_at_the_chosen_setting() {
    let mut app = connected();
    app.update(ch('7'));
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    match &app.modal {
        Some(Modal::Form(form)) => {
            assert_eq!(form.kind, FormKind::Mesh);
            assert_eq!(form.fields[form.focus].key, "commit_timeout");
        }
        other => panic!("expected the network form, got {other:?}"),
    }
}

#[test]
fn a_tunnel_gets_its_own_trunk_in_the_settings() {
    let mut app = connected();
    app.update(ch('7'));
    // The fixed settings come first, then one row per tunnel (buero, haus2, ...).
    app.settings_selected = crate::tui::ui::settings::ROWS.len() + 1;
    app.update(key(KeyCode::Enter));
    assert!(matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::TunnelTrunk(2)));
    for c in "ens20".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none(), "saved");
    let draft = app.draft.as_ref().unwrap();
    assert_eq!(
        draft.local.tunnel_trunks.get("haus2").map(String::as_str),
        Some("ens20")
    );
    assert!(app.draft_dirty());
    // Empty again: back to the default trunk.
    app.update(key(KeyCode::Enter));
    for _ in 0..5 {
        app.update(key(KeyCode::Backspace));
    }
    app.update(key(KeyCode::Enter));
    assert!(app.draft.as_ref().unwrap().local.tunnel_trunks.is_empty());
}

#[test]
fn a_restricted_site_sets_its_own_vlans() {
    let mut app = restricted();
    app.update(ch('3'));
    app.update(ch('o'));
    assert!(matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::OwnVlans(0)));
    for c in "50".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none(), "saved");
    let heim = app
        .draft
        .as_ref()
        .unwrap()
        .mesh
        .site("heim")
        .unwrap()
        .clone();
    assert_eq!(heim.own_vlans.map(|v| v.to_string()).as_deref(), Some("50"));
    // The rights allow it: the review opens.
    app.update(ch('a'));
    assert!(
        matches!(app.modal, Some(Modal::Review(_))),
        "{:?}",
        app.message
    );
}
