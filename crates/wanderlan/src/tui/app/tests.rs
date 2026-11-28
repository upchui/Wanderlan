use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wanderlan_core::api::{ChangeSource, Event, Level, PendingChange, Request, SiteState};
use wanderlan_core::config::VlanSet;

use super::*;
use crate::client::ClientError;
use crate::tui::fixtures::{self, id};

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
fn after_a_reset_everything_is_reloaded_and_the_old_draft_is_gone() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.network.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
    }
    let fx = app.update(Msg::Response {
        request: Request::Reset,
        result: Ok(Reply::Reset(ResetReport::default())),
    });
    let reqs = requests(&fx);
    assert!(reqs.contains(&&Request::Interfaces));
    assert!(reqs.contains(&&Request::Status));
    assert!(app.draft.is_none(), "nothing of the old network to apply");
    let fx = app.update(Msg::Response {
        request: Request::Interfaces,
        result: Ok(Reply::Interfaces(vec![])),
    });
    assert!(fx.is_empty());
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
        d.network.vlans = VlanSet::from_range(10, 60);
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
        d.network.vlans = VlanSet::from_range(10, 60);
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
    assert!(apply.config.network.contains("vlans = \"10-60\""));
    assert!(
        apply
            .config
            .local
            .contains(&format!("self = \"{}\"  # heim", id("heim")))
    );
    assert_eq!(app.busy, Some(Busy::Applying));
}

#[test]
fn a_change_that_was_rolled_back_comes_back_into_the_draft() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.network.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
    }
    app.update(ch('a'));
    app.update(key(KeyCode::Enter));
    app.update(reply(
        Request::Apply(ApplyParams {
            config: ConfigInput {
                network: String::new(),
                local: String::new(),
            },
            base_revision: None,
            timeout_secs: None,
        }),
        Reply::Applied(Applied {
            confirm_within_secs: 60,
            diff: String::new(),
            changes: Vec::new(),
            warnings: Vec::new(),
        }),
    ));
    with_pending(&mut app, ChangeSource::Local);
    // The countdown ran out: no pending change, the old configuration runs.
    app.update(reply(
        Request::Status,
        Reply::Status(Box::new(fixtures::status())),
    ));
    app.update(reply(
        Request::GetConfig,
        Reply::Config(fixtures::config_texts()),
    ));
    assert!(app.draft_dirty(), "the edit is back");
    assert_eq!(
        app.draft.as_ref().map(|d| d.network.vlans.to_string()),
        Some("10-60".into())
    );
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("rolled back")),
        "{:?}",
        app.message
    );
}

#[test]
fn t_optimizes_the_tunnel_to_the_selected_peer() {
    let mut app = connected();
    // Fixed by the setting (off, the default): it says so, nothing is asked.
    let fx = app.update(ch('t'));
    assert!(requests(&fx).is_empty());
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("RPS is fixed by the setting here")),
        "{:?}",
        app.message
    );
    // auto: measured with the peer.
    if let Some(running) = app.running.as_mut() {
        running.local.rps_transport = wanderlan_core::config::RpsMode::Auto;
    }
    let fx = app.update(ch('t'));
    assert_eq!(
        requests(&fx),
        [&Request::Tune(wanderlan_core::api::TuneParams {
            site: "buero".into()
        })]
    );
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("Optimizing the tunnel to buero")),
        "{:?}",
        app.message
    );
    // The answer says what was kept.
    app.update(reply(
        Request::Tune(wanderlan_core::api::TuneParams {
            site: "buero".into(),
        }),
        Reply::Tuned(wanderlan_core::api::Tune {
            peer: "buero".into(),
            here: wanderlan_core::api::TuneSide {
                nic: Some("ens18".into()),
                rps: Some(false),
                off_bps: 1_400_000_000,
                on_bps: 900_000_000,
                why_not: None,
            },
            there: wanderlan_core::api::TuneSide {
                nic: Some("ens18".into()),
                rps: Some(true),
                off_bps: 1_000_000_000,
                on_bps: 1_300_000_000,
                why_not: None,
            },
        }),
    ));
    assert!(
        app.message.as_ref().is_some_and(|m| m
            .text
            .contains("Tunnel to buero optimized: here RPS off, there RPS on")),
        "{:?}",
        app.message
    );
}

#[test]
fn edits_made_during_the_countdown_survive_a_rollback() {
    let mut app = connected();
    let mut applied = fixtures::config_texts();
    if let Some(d) = &mut app.draft {
        d.network.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
        applied.network = wanderlan_core::config::render_network(&d.network);
        applied.revision = 8;
    }
    app.update(ch('a'));
    app.update(key(KeyCode::Enter));
    app.update(reply(
        Request::Apply(ApplyParams {
            config: ConfigInput {
                network: String::new(),
                local: String::new(),
            },
            base_revision: None,
            timeout_secs: None,
        }),
        Reply::Applied(Applied {
            confirm_within_secs: 60,
            diff: String::new(),
            changes: Vec::new(),
            warnings: Vec::new(),
        }),
    ));
    with_pending(&mut app, ChangeSource::Local);
    // The applied change runs; during the countdown buero gets VLAN 20.
    app.update(reply(Request::GetConfig, Reply::Config(applied)));
    if let Some(d) = &mut app.draft {
        let buero = d
            .network
            .sites
            .iter_mut()
            .find(|s| s.name == "buero")
            .unwrap();
        buero.vlans = Some(VlanSet::from_range(20, 20));
        d.revalidate();
    }
    // Rolled back by the timer: both edits are in the draft again.
    app.update(reply(
        Request::Status,
        Reply::Status(Box::new(fixtures::status())),
    ));
    app.update(reply(
        Request::GetConfig,
        Reply::Config(fixtures::config_texts()),
    ));
    let d = app.draft.as_ref().unwrap();
    assert_eq!(d.network.vlans.to_string(), "10-60");
    assert_eq!(
        d.network.by_name("buero").unwrap().vlans,
        Some(VlanSet::from_range(20, 20))
    );
    assert_eq!(d.base_revision, 7, "it builds on what runs");
}

#[test]
fn a_failed_confirm_after_the_countdown_brings_the_change_back() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.network.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
    }
    app.update(ch('a'));
    app.update(key(KeyCode::Enter));
    app.update(reply(
        Request::Apply(ApplyParams {
            config: ConfigInput {
                network: String::new(),
                local: String::new(),
            },
            base_revision: None,
            timeout_secs: None,
        }),
        Reply::Applied(Applied {
            confirm_within_secs: 60,
            diff: String::new(),
            changes: Vec::new(),
            warnings: Vec::new(),
        }),
    ));
    with_pending(&mut app, ChangeSource::Local);
    // c at the last second: the daemon rolled back meanwhile.
    app.update(ch('c'));
    let fx = app.update(Msg::Response {
        request: Request::Confirm,
        result: Err(ClientError::Api(ApiError::new(
            "there is no change to confirm",
        ))),
    });
    assert!(requests(&fx).contains(&&Request::GetConfig));
    app.update(reply(
        Request::GetConfig,
        Reply::Config(fixtures::config_texts()),
    ));
    assert_eq!(
        app.draft.as_ref().map(|d| d.network.vlans.to_string()),
        Some("10-60".into()),
        "the change is kept in the draft"
    );
}

#[test]
fn invalid_draft_is_not_applied() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.network.sites[2].vlans = Some(wanderlan_core::config::VlanSet::new());
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
    // r asks first; y rolls back.
    let fx = app.update(ch('r'));
    assert!(requests(&fx).is_empty());
    let fx = app.update(ch('y'));
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
    let text = banner_text(&local, false);
    assert!(text.contains("UNCONFIRMED CHANGE: rollback in 9 s"));
    assert!(text.contains("c confirm"));
    let synced = PendingChange {
        remaining_secs: 30,
        source: ChangeSource::Sync {
            peer: "buero".into(),
        },
    };
    assert!(banner_text(&synced, false).contains("CHANGE FROM buero"));
    assert!(banner_text(&local, true).contains("Esc, then c"));
    // A change from a peer settles by itself: no keys offered.
    assert!(banner_text(&synced, false).contains("confirmed by itself once buero answers"));
    assert!(!banner_text(&synced, false).contains("c confirm"));
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
        d.network.vlans = VlanSet::from_range(10, 60);
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
    assert_eq!(sent[0].changes(), wanderlan_core::config::Changes::Server);
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
        .and_then(|d| d.network.sites.iter().position(|s| s.name == "haus3"))
        .unwrap();
    app.sites_selected = haus3;
    app.update(ch('o'));
    assert!(app.draft_dirty());
    let site = app
        .draft
        .as_ref()
        .unwrap()
        .network
        .by_name("haus3")
        .unwrap();
    assert!(!site.online);
}

#[test]
fn a_client_that_changes_its_tunnel_itself_is_not_changed_by_its_server() {
    let mut app = connected();
    let mut texts = fixtures::config_texts();
    assert!(texts.network.contains("changes = \"server\""));
    texts.network = texts
        .network
        .replace("changes = \"server\"", "changes = \"client\"");
    app.update(reply(Request::GetConfig, Reply::Config(texts)));
    app.update(ch('2'));
    let haus6 = app
        .draft
        .as_ref()
        .and_then(|d| d.network.sites.iter().position(|s| s.name == "haus6"))
        .unwrap();
    app.sites_selected = haus6;
    app.update(ch('o'));
    assert!(!app.draft_dirty(), "only haus6 changes its tunnel");
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("only \"haus6\" itself")),
        "{:?}",
        app.message
    );
    // Where it is stays for its server: the form opens for that.
    app.update(ch('e'));
    assert!(matches!(app.modal, Some(Modal::Form(_))));
}

/// The fixture app on a site that is managed by "buero" (restricted).
fn restricted() -> App {
    let mut app = connected();
    let mut texts = fixtures::config_texts();
    // heim a client of buero, which alone changes its tunnel.
    let heim = "name = \"heim\"\naddress = \"10.8.0.1\"\n";
    assert!(texts.network.contains(heim), "{}", texts.network);
    texts.network = texts.network.replace(
        heim,
        &format!("{heim}server = \"buero\"\nchanges = \"server\"\n"),
    );
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
    // The network's VLANs, in the VLAN view and in Sites (m).
    app.update(ch('3'));
    app.update(ch('v'));
    assert!(refused(&app));
    app.message = None;
    app.update(ch('2'));
    app.update(ch('m'));
    assert!(refused(&app));
    // Its server (a client changes nothing above it).
    app.message = None;
    app.update(ch('2'));
    app.sites_selected = 1;
    app.update(ch('e'));
    assert!(refused(&app));
    // The settings of this host stay its own.
    app.update(ch('8'));
    app.settings_selected = 2;
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
    // The graphs: first the whole recorded hour, then what is new.
    let asked = |fx: &[Effect]| {
        fx.iter().find_map(|e| match e {
            Effect::Request(Request::History(p)) => Some(p.since_ms),
            _ => None,
        })
    };
    let fx = app.update(Msg::Tick);
    assert_eq!(asked(&fx), Some(None));
    app.update(reply(
        Request::History(HistoryParams::default()),
        Reply::Traffic(Box::new(fixtures::history())),
    ));
    assert_eq!(
        app.recorded.peers.get("buero").map(|p| p.traffic.len()),
        Some(300)
    );
    let fx = app.update(Msg::Tick);
    assert_eq!(asked(&fx), Some(Some(fixtures::HISTORY_NOW_MS)));
    // z: another time range.
    app.update(ch('z'));
    assert_eq!(app.graph_range, crate::tui::history::Range::Long);
    app.update(key(KeyCode::Enter));
    assert!(matches!(&app.modal, Some(Modal::Peer { name, .. }) if name == "buero"));
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
}

#[test]
fn settings_open_the_form_at_the_chosen_setting() {
    let mut app = connected();
    app.update(ch('8'));
    // This site, Trunk, then Commit timeout: a setting of this host.
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    match &app.modal {
        Some(Modal::Form(form)) => {
            assert_eq!(form.kind, FormKind::Local);
            assert_eq!(form.fields[form.focus].key, "commit_timeout");
        }
        other => panic!("expected the form of this host, got {other:?}"),
    }
    // The ports (after the tuning of this host): the own entry of this site.
    app.update(key(KeyCode::Esc));
    app.settings_selected = 8;
    app.update(key(KeyCode::Enter));
    assert!(
        matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::OwnPorts(id("heim"))),
        "{:?}",
        app.modal
    );
}

fn picker(app: &App) -> &PortPicker {
    match &app.modal {
        Some(Modal::Ports(p)) => p,
        other => panic!("expected the trunk port window, got {other:?}"),
    }
}

#[test]
fn a_tunnel_gets_its_own_trunk_in_the_settings() {
    let mut app = connected();
    app.update(ch('8'));
    // The fixed settings come first, then one row per tunnel (buero, haus2, ...).
    app.settings_selected = crate::tui::ui::settings::ROWS.len() + 1;
    let fx = app.update(key(KeyCode::Enter));
    assert!(
        requests(&fx).contains(&&Request::Interfaces),
        "the list is read"
    );
    assert_eq!(picker(&app).target, PortTarget::Tunnel(id("haus2")));
    app.update(reply(
        Request::Interfaces,
        Reply::Interfaces(fixtures::interfaces()),
    ));
    // Rows: br-lan, ens18, ens19, eth1, then "other". The tunnel to buero
    // runs over ens18: it can be chosen all the same, with a warning.
    app.update(key(KeyCode::Down));
    app.update(ch(' '));
    assert_eq!(picker(&app).chosen, ["ens18"]);
    assert!(picker(&app).warning(&app.interfaces).is_some());
    app.update(ch(' '));
    assert!(picker(&app).chosen.is_empty(), "and taken out again");
    // The default trunk's only port can be chosen, which is the default
    // trunk itself; taken out again.
    app.update(key(KeyCode::Down));
    app.update(ch(' '));
    assert_eq!(picker(&app).chosen, ["ens19"]);
    app.update(ch(' '));
    assert!(picker(&app).chosen.is_empty());
    // A name that is not there yet: typed in the last row.
    app.update(key(KeyCode::End));
    for c in "ens20".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    assert_eq!(picker(&app).chosen, ["ens20"], "Enter adds it");
    // A setting of this host: applied at once, the draft stays as it was.
    let fx = app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none(), "applied");
    let local = applied_local(&fx);
    assert!(
        local.contains(&format!("[tunnels.\"{}\"]\ntrunk = \"ens20\"", id("haus2"))),
        "{local}"
    );
    assert!(!app.draft_dirty());
}

/// The local.toml of the configuration applied with `fx`.
fn applied_local(fx: &[Effect]) -> String {
    requests(fx)
        .into_iter()
        .find_map(|r| match r {
            Request::Apply(p) => Some(p.config.local.clone()),
            _ => None,
        })
        .expect("an apply")
}

#[test]
fn a_setting_is_applied_when_saved_and_the_draft_keeps_its_edits() {
    let mut app = connected();
    // An edit in the draft, not applied.
    let running = app.running.as_ref().and_then(Draft::from_running).unwrap();
    let site = &mut app.draft.as_mut().unwrap().network.sites[1];
    site.online = !site.online;
    let draft = app.draft.clone();
    app.update(ch('8'));
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    for c in "2m".chars() {
        app.update(ch(c));
    }
    let fx = app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none(), "{:?}", app.message);
    let Some(Request::Apply(params)) = requests(&fx).into_iter().next() else {
        panic!("an apply: {fx:?}");
    };
    // The running configuration with that one change.
    assert!(params.config.local.contains("commit_timeout = \"2m\""));
    assert_eq!(params.config.network, running.network_text());
    assert_eq!(params.base_revision, Some(7));
    assert_eq!(app.draft, draft, "the draft keeps its edits");
    // While a change waits for confirmation, a setting is not applied.
    app.update(reply(
        Request::Apply(params.clone()),
        Reply::Applied(Applied {
            confirm_within_secs: 60,
            diff: String::new(),
            changes: Vec::new(),
            warnings: Vec::new(),
        }),
    ));
    with_pending(&mut app, ChangeSource::Local);
    app.update(key(KeyCode::Enter));
    for c in "3m".chars() {
        app.update(ch(c));
    }
    let fx = app.update(key(KeyCode::Enter));
    assert!(
        requests(&fx)
            .iter()
            .all(|r| !matches!(r, Request::Apply(_)))
    );
    assert!(
        matches!(app.modal, Some(Modal::Form(_))),
        "the form stays open"
    );
}

#[test]
fn several_trunk_ports_with_a_main_port() {
    let mut app = connected();
    app.update(reply(
        Request::Interfaces,
        Reply::Interfaces(fixtures::interfaces()),
    ));
    app.update(ch('2'));
    app.update(ch('l'));
    assert_eq!(picker(&app).target, PortTarget::Default);
    assert_eq!(picker(&app).chosen, ["ens19"]);
    // eth1 is in br-lan: not a port.
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    app.update(ch(' '));
    assert!(picker(&app).error.as_deref().unwrap().contains("in br-lan"));
    app.update(key(KeyCode::Down));
    for c in "usb0".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    // usb0 now sits above "other"; m makes it the main port.
    app.update(key(KeyCode::Up));
    app.update(ch('m'));
    assert_eq!(picker(&app).chosen, ["usb0", "ens19"]);
    // Esc asks first; n goes back to the window.
    app.update(key(KeyCode::Esc));
    assert!(matches!(
        &app.modal,
        Some(Modal::Ask(a)) if matches!(a.action, AskAction::DiscardPorts(_))
    ));
    app.update(ch('n'));
    assert_eq!(picker(&app).chosen, ["usb0", "ens19"], "nothing lost");
    let fx = app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert!(applied_local(&fx).contains("trunk = [\"usb0\", \"ens19\"]"));
    // The daemon has not answered yet: the picker shows what runs.
    app.busy = None;
    // The default trunk needs a port. Rows: br-lan, ens18, ens19, eth1.
    app.update(ch('l'));
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    app.update(ch(' '));
    assert!(picker(&app).chosen.is_empty());
    app.update(key(KeyCode::Enter));
    assert!(
        picker(&app)
            .error
            .as_deref()
            .unwrap()
            .contains("at least one port")
    );
}

#[test]
fn a_restricted_site_sets_its_own_vlans() {
    let mut app = restricted();
    app.update(ch('3'));
    app.update(ch('o'));
    assert!(matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::OwnVlans(id("heim"))));
    for c in "50".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none(), "saved");
    let heim = app
        .draft
        .as_ref()
        .unwrap()
        .network
        .by_name("heim")
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

/// The fixture app on a client that alone changes its tunnel ("only the
/// client").
fn changes_itself() -> App {
    let mut app = connected();
    let mut texts = fixtures::config_texts();
    let heim = "name = \"heim\"\naddress = \"10.8.0.1\"\n";
    texts.network = texts.network.replace(
        heim,
        &format!("{heim}server = \"buero\"\nchanges = \"client\"\n"),
    );
    app.update(reply(Request::GetConfig, Reply::Config(texts)));
    app
}

#[test]
fn a_client_that_changes_its_own_tunnel_edits_vlans_and_mappings() {
    let mut app = changes_itself();
    assert_eq!(app.restricted_by(), None);
    app.update(ch('3'));
    app.update(ch('v'));
    assert!(matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::Vlans));
    app.update(key(KeyCode::Esc));
    app.update(ch('n'));
    assert!(matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::Map(None)));
    // Site heim, source VLAN 30, mapped VLAN "native" (prefilled).
    app.update(key(KeyCode::Tab));
    for c in "30".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none(), "saved: {:?}", app.message);
    app.update(ch('a'));
    assert!(
        matches!(app.modal, Some(Modal::Review(_))),
        "{:?}",
        app.message
    );
}

#[test]
fn page_keys_and_the_overview_cursor() {
    let press = |app: &mut App, code| {
        app.update(key(code));
    };
    let mut app = connected();
    // The cursor runs from the peers into the sites of the network.
    let peers = app.status.as_ref().map_or(0, |s| s.peers.len());
    press(&mut app, KeyCode::End);
    let sites = app.overview_sites().len();
    assert_eq!(app.overview_selected, peers + sites - 1);
    press(&mut app, KeyCode::Home);
    assert_eq!(app.overview_selected, 0);
    // Enter on a site shows its details.
    for _ in 0..peers {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Enter);
    let first = app.overview_sites()[0].name.clone();
    assert!(matches!(&app.modal, Some(Modal::Site { name, .. }) if *name == first));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('2'));
    // End and Home in the sites (tree order: haus6 is the second row).
    press(&mut app, KeyCode::End);
    assert_eq!(
        app.draft.as_ref().unwrap().network.sites[app.sites_selected].name,
        "haus5"
    );
    press(&mut app, KeyCode::Home);
    assert_eq!(app.sites_selected, 0);
}

#[test]
fn confirm_and_roll_back_work_in_windows() {
    let mut app = connected();
    with_pending(&mut app, ChangeSource::Local);
    app.update(ch('?'));
    assert!(matches!(app.modal, Some(Modal::Help { .. })));
    let fx = app.update(ch('c'));
    assert!(
        requests(&fx).contains(&&Request::Confirm),
        "c confirms from the help"
    );
    assert!(app.modal.is_none());
    // r asks first, also from a window.
    app.busy = None;
    app.update(ch('?'));
    app.update(ch('r'));
    assert!(matches!(
        &app.modal,
        Some(Modal::Ask(a)) if a.action == AskAction::Rollback
    ));
    let fx = app.update(key(KeyCode::Enter));
    assert!(requests(&fx).is_empty(), "Enter is no for a roll back");
    app.update(ch('r'));
    let fx = app.update(ch('y'));
    assert!(requests(&fx).contains(&&Request::Rollback));
}

#[test]
fn enter_is_no_where_a_wrong_yes_hurts() {
    let mut app = connected();
    app.update(ch('6'));
    app.update(ch('R'));
    assert!(matches!(
        &app.modal,
        Some(Modal::Ask(a)) if a.action == AskAction::Reset
    ));
    let fx = app.update(key(KeyCode::Enter));
    assert!(requests(&fx).is_empty(), "Enter does not reset");
    assert!(app.modal.is_none());
}

#[test]
fn a_form_with_input_asks_before_it_closes() {
    let mut app = connected();
    app.update(ch('2'));
    app.update(ch('n'));
    // Esc on an untouched form closes it.
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
    app.update(ch('n'));
    app.update(ch('x'));
    app.update(key(KeyCode::Esc));
    assert!(matches!(&app.modal, Some(Modal::Ask(_))));
    app.update(ch('n'));
    match &app.modal {
        Some(Modal::Form(form)) => {
            assert_eq!(form.fields[0].value(), "x", "the input is still there")
        }
        other => panic!("back to the form, got {other:?}"),
    }
    app.update(key(KeyCode::Esc));
    app.update(ch('y'));
    assert!(app.modal.is_none());
}

#[test]
fn deleting_an_invitation_asks() {
    let mut app = connected();
    app.update(ch('2'));
    app.update(ch('I'));
    app.update(reply(
        Request::Invites,
        Reply::Invites(vec![wanderlan_core::api::Invitation {
            code: "K7F3Q-9QX2M".into(),
            command: "wanderlan join 10.8.0.1 K7F3Q-9QX2M".into(),
            created_secs: 0,
            in_use: false,
            rules: wanderlan_core::api::SiteRules::default(),
            expires_secs: None,
        }]),
    ));
    app.update(ch('d'));
    assert!(matches!(&app.modal, Some(Modal::Ask(_))));
    app.update(ch('n'));
    assert!(
        matches!(&app.modal, Some(Modal::Invites { .. })),
        "no: back to the list"
    );
    app.update(ch('d'));
    let fx = app.update(ch('y'));
    assert!(
        requests(&fx)
            .iter()
            .any(|r| matches!(r, Request::DeleteInvite(_)))
    );
    assert!(
        matches!(&app.modal, Some(Modal::Invites { .. })),
        "yes: back to the list"
    );
}

#[test]
fn deleting_a_site_removes_that_site_even_if_the_draft_changed_meanwhile() {
    let mut app = connected();
    app.update(ch('2'));
    let at = |app: &App, name: &str| {
        app.draft
            .as_ref()
            .unwrap()
            .network
            .sites
            .iter()
            .position(|s| s.name == name)
    };
    app.sites_selected = at(&app, "haus3").unwrap();
    app.update(ch('d'));
    assert!(matches!(
        &app.modal,
        Some(Modal::Ask(a)) if a.action == AskAction::DeleteSite(id("haus3"))
    ));
    // Meanwhile another site goes (e.g. a sync): the indices move.
    let buero = at(&app, "buero").unwrap();
    app.draft.as_mut().unwrap().remove_site(buero);
    app.update(ch('y'));
    assert_eq!(at(&app, "haus3"), None, "haus3 is gone");
    assert!(at(&app, "haus2").is_some(), "not the site at its old index");
}

#[test]
fn the_tunnel_search_can_be_changed_when_nothing_matches() {
    let mut app = connected();
    app.update(ch('3'));
    app.update(ch('t'));
    app.update(ch('/'));
    for c in "zzz".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    assert!(app.visible_tunnels().is_empty());
    assert!(!app.tunnels_filter.editing);
    app.update(ch('/'));
    assert!(app.tunnels_filter.editing, "/ works without a row");
}

#[test]
fn joining_ends_with_a_message_when_the_connection_is_lost() {
    let mut app = connected();
    app.wizard.step = Step::Joining;
    app.update(Msg::Disconnected(ClientError::Closed));
    assert_eq!(app.wizard.step, Step::Summary);
    assert!(app.wizard.error.as_deref().unwrap().contains("connection"));
}

#[test]
fn an_unanswered_request_is_sent_again_after_a_while() {
    let mut app = connected();
    let fx = app.update(ch('4'));
    assert!(requests(&fx).contains(&&Request::MacTable));
    // No answer: after a few seconds it is asked again.
    let again = (0..6)
        .flat_map(|_| app.update(Msg::Tick))
        .filter(|e| matches!(e, Effect::Request(Request::MacTable)))
        .count();
    assert!(again >= 1, "asked again");
}

#[test]
fn the_peer_details_go_to_the_next_peer() {
    let mut app = connected();
    app.update(key(KeyCode::Enter));
    assert!(matches!(&app.modal, Some(Modal::Peer { name, .. }) if name == "buero"));
    app.update(key(KeyCode::Right));
    assert!(matches!(&app.modal, Some(Modal::Peer { name, .. }) if name == "haus2"));
    app.update(key(KeyCode::Left));
    app.update(key(KeyCode::Left));
    assert!(matches!(&app.modal, Some(Modal::Peer { name, .. }) if name == "buero"));
}

#[test]
fn from_the_peer_details_to_one_vlan_and_back() {
    let mut app = connected();
    let fx = app.update(key(KeyCode::Enter));
    assert!(
        requests(&fx).contains(&&Request::VlanTraffic),
        "the VLANs are read right away"
    );
    app.update(reply(
        Request::VlanTraffic,
        Reply::VlanTraffic(fixtures::vlan_traffic()),
    ));
    // buero's VLANs, those that carried traffic first: 10, 20, then 30.
    app.update(key(KeyCode::Down));
    assert!(matches!(
        &app.modal,
        Some(Modal::Peer { vlan: Some(20), .. })
    ));
    app.update(key(KeyCode::Up));
    assert!(matches!(
        &app.modal,
        Some(Modal::Peer { vlan: Some(10), .. })
    ));
    let fx = app.update(key(KeyCode::Enter));
    assert!(
        matches!(&app.modal, Some(Modal::Vlan { vid: 10, back: Some(b) }) if b == "buero"),
        "{:?}",
        app.modal
    );
    assert!(requests(&fx).contains(&&Request::MacTable), "its MACs");
    // Up/Down: the VLANs of this site in order.
    app.update(key(KeyCode::Down));
    assert!(matches!(&app.modal, Some(Modal::Vlan { vid: 20, .. })));
    // Refreshed while open (once the last request is answered).
    app.update(reply(
        Request::VlanTraffic,
        Reply::VlanTraffic(fixtures::vlan_traffic()),
    ));
    let polled = (0..4)
        .flat_map(|_| app.update(Msg::Tick))
        .filter(|e| matches!(e, Effect::Request(Request::VlanTraffic)))
        .count();
    assert!(polled >= 1, "polled while the window is open");
    // Esc goes back to the peer, with the VLAN chosen.
    app.update(key(KeyCode::Esc));
    assert!(matches!(&app.modal, Some(Modal::Peer { name, vlan: Some(20) }) if name == "buero"));
    // m in the VLAN window: the MAC table of this VLAN.
    app.update(key(KeyCode::Enter));
    app.update(ch('m'));
    assert!(app.modal.is_none());
    assert_eq!(app.view, View::Macs);
    assert_eq!(app.macs.filter.value(), "vlan:20");
}

#[test]
fn the_vlan_history_follows_the_answers() {
    let mut app = connected();
    // Only the VLAN the window shows is recorded.
    app.modal = Some(Modal::Vlan {
        vid: 20,
        back: None,
    });
    // One point per sample of the daemon: the same sample again, or one
    // without rates, adds none.
    let sampled = |at: Option<u64>| VlanTraffic {
        sampled_ms: at,
        ..fixtures::vlan_traffic()
    };
    for at in [Some(2_000), Some(2_000), None, Some(4_000), Some(6_000)] {
        app.update(reply(Request::VlanTraffic, Reply::VlanTraffic(sampled(at))));
    }
    let samples = app.vlan_history.get(&20).expect("VLAN 20");
    assert_eq!(samples.len(), 3);
    assert_eq!(samples[1].0, 4_000, "at the time of the sample");
    // Over the tunnels: vx-buero 2.4M + vx-haus2 300k in, 1.8M + 300k out
    // (the trunk ens19 is not counted: in and out would be the same).
    assert_eq!(samples[0].1, 2_700_000);
    assert_eq!(samples[0].2, 2_100_000);
    // An answer without ports (a read error) keeps the history.
    app.update(reply(
        Request::VlanTraffic,
        Reply::VlanTraffic(VlanTraffic::default()),
    ));
    assert_eq!(app.vlan_history.get(&20).map(VecDeque::len), Some(3));
    assert_eq!(app.vlan_history.len(), 1, "only VLAN 20");
    // Closed: nothing kept.
    app.modal = None;
    app.update(reply(
        Request::VlanTraffic,
        Reply::VlanTraffic(fixtures::vlan_traffic()),
    ));
    assert!(app.vlan_history.is_empty());
}

#[test]
fn a_restricted_site_renames_itself_but_not_its_manager() {
    let mut app = restricted();
    app.update(ch('2'));
    let index = |app: &App, name: &str| {
        app.draft
            .as_ref()
            .and_then(|d| d.network.sites.iter().position(|s| s.name == name))
            .unwrap()
    };
    // heim is managed by buero (restricted), but its name is its own.
    let heim = index(&app, "heim");
    app.sites_selected = heim;
    app.update(ch('N'));
    assert!(
        matches!(&app.modal, Some(Modal::Form(f)) if f.kind == FormKind::Rename(id("heim"))),
        "{:?}",
        app.message
    );
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
    // buero may not be renamed from here.
    app.sites_selected = index(&app, "buero");
    app.update(ch('N'));
    assert!(app.modal.is_none());
    assert!(
        app.message
            .as_ref()
            .is_some_and(|m| m.text.contains("only \"buero\" itself may")),
        "{:?}",
        app.message
    );
}
