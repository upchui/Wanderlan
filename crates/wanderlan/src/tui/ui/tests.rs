//! Snapshot tests of every screen at 80×24.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use wanderlan_core::api::{
    ChangeSource, Invitation, PendingChange, Request, SiteRules, Translation,
};
use wanderlan_core::config::{VlanSet, parse_network, render_network};

use super::draw;
use crate::client::ClientError;
use crate::tui::app::{App, MessageKind, Msg, Reply};
use crate::tui::fixtures;

fn render(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal.draw(|f| draw(f, app)).expect("draw");
    terminal.backend().to_string()
}

fn key(app: &mut App, code: KeyCode) {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn reply(app: &mut App, request: Request, reply: Reply) {
    app.update(Msg::Response {
        request,
        result: Ok(reply),
    });
}

fn connected() -> App {
    let mut app = App::new("/run/wanderlan/wanderlan.sock", false);
    app.start();
    app.update(Msg::Connected);
    reply(
        &mut app,
        Request::Status,
        Reply::Status(Box::new(fixtures::status())),
    );
    reply(
        &mut app,
        Request::GetConfig,
        Reply::Config(fixtures::config_texts()),
    );
    reply(
        &mut app,
        Request::Subscribe,
        Reply::History(fixtures::events()),
    );
    app
}

#[test]
fn overview() {
    insta::assert_snapshot!(render(&connected()));
}

#[test]
fn overview_with_pending_change() {
    let mut app = connected();
    let mut status = fixtures::status();
    status.pending = Some(PendingChange {
        remaining_secs: 37,
        source: ChangeSource::Sync {
            peer: "buero".into(),
        },
    });
    reply(&mut app, Request::Status, Reply::Status(Box::new(status)));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn sites() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('2'));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn vlans_with_conflict() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        // Two local VLANs onto shared VLAN 20 at haus3.
        d.network.maps[1].shared = 20;
        d.revalidate();
    }
    key(&mut app, KeyCode::Char('3'));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn mac_table() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('4'));
    reply(&mut app, Request::MacTable, Reply::Macs(fixtures::macs()));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn log() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('5'));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn help() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('?'));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn review_diff() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.network.vlans = VlanSet::from_range(10, 60);
        d.revalidate();
    }
    key(&mut app, KeyCode::Char('a'));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn wizard_welcome() {
    let mut app = App::new("/run/wanderlan/wanderlan.sock", false);
    app.start();
    app.update(Msg::Connected);
    reply(
        &mut app,
        Request::Status,
        Reply::Status(Box::new(fixtures::unconfigured_status())),
    );
    reply(
        &mut app,
        Request::GetConfig,
        Reply::Config(fixtures::unconfigured_texts()),
    );
    reply(
        &mut app,
        Request::Interfaces,
        Reply::Interfaces(fixtures::interfaces()),
    );
    insta::assert_snapshot!(render(&app));
}

#[test]
fn daemon_not_running() {
    let mut app = App::new("/run/wanderlan/wanderlan.sock", false);
    app.start();
    app.update(Msg::ConnectFailed(ClientError::NotRunning {
        path: PathBuf::from("/run/wanderlan/wanderlan.sock"),
    }));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn settings_view() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('7'));
    key(&mut app, KeyCode::Down);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn settings_trunk_per_tunnel() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.local.tunnel_trunks.insert("haus2".into(), "ens20".into());
    }
    key(&mut app, KeyCode::Char('7'));
    // Down to "to haus2": the fixed rows, then buero, then haus2.
    for _ in 0..super::settings::ROWS.len() + 1 {
        key(&mut app, KeyCode::Down);
    }
    insta::assert_snapshot!(render(&app));
}

#[test]
fn peer_details() {
    let mut app = connected();
    for _ in 0..30 {
        reply(
            &mut app,
            Request::Status,
            Reply::Status(Box::new(fixtures::status())),
        );
    }
    key(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn invitations() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('I'));
    let invitation = |code: &str, rules: SiteRules| Invitation {
        code: code.into(),
        command: format!("wanderlan join 10.8.0.1 {code}"),
        created_secs: 1_795_514_400,
        in_use: false,
        rules,
    };
    reply(
        &mut app,
        Request::Invites,
        Reply::Invites(vec![
            invitation("K7F3Q-9QX2M", SiteRules::default()),
            invitation(
                "M0K8J-S8T4F",
                SiteRules {
                    name: Some("haus7".into()),
                    vlans: Some("native,20,30".into()),
                    native: None,
                    translations: vec![Translation {
                        local: 130,
                        shared: 30,
                    }],
                    allow_self: true,
                },
            ),
        ]),
    );
    key(&mut app, KeyCode::Down);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn invite_form() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('i'));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn overview_with_refused_client() {
    let mut app = connected();
    let mut status = fixtures::status();
    status.peers[3].refused = Some("site \"heim\" is already used".into());
    reply(&mut app, Request::Status, Reply::Status(Box::new(status)));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn peer_details_refused() {
    let mut app = connected();
    let mut status = fixtures::status();
    status.peers[0].refused = Some("site \"heim\" is already used".into());
    reply(&mut app, Request::Status, Reply::Status(Box::new(status)));
    key(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn overview_with_sync_problem() {
    let mut app = connected();
    let mut status = fixtures::status();
    status.sync_problem =
        Some("Your change was not taken over by \"buero\": site \"haus9\" is already used".into());
    reply(&mut app, Request::Status, Reply::Status(Box::new(status)));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn sites_as_tree() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        // haus2 becomes a client of heim: it moves up below heim, next to haus6.
        d.network.sites[2].uplink = Some("heim".into());
        d.revalidate();
    }
    key(&mut app, KeyCode::Char('2'));
    // Down twice from heim: haus2, then haus6 (the tree order, not the file order).
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    assert_eq!(app.sites_selected, 5);
    key(&mut app, KeyCode::Down);
    assert_eq!(
        app.sites_selected, 1,
        "after the clients of heim comes buero"
    );
    key(&mut app, KeyCode::Up);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn vlans_tunnels() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        d.network
            .set_tunnel_limit("heim", "buero", Some(VlanSet::from_range(20, 30)));
        d.revalidate();
    }
    key(&mut app, KeyCode::Char('3'));
    key(&mut app, KeyCode::Char('t'));
    insta::assert_snapshot!(render(&app));
    // d lifts the limit of the selected tunnel (buero – heim is the fourth).
    for _ in 0..3 {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Char('d'));
    assert!(
        app.draft
            .as_ref()
            .is_some_and(|d| d.network.tunnels.is_empty()),
        "the limit is gone"
    );
}

#[test]
fn tunnel_vlans_form() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('3'));
    key(&mut app, KeyCode::Char('t'));
    key(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn peer_details_limited() {
    let mut app = connected();
    let mut texts = fixtures::config_texts();
    let mut network = parse_network(&texts.network).unwrap().network.unwrap();
    network.set_tunnel_limit("heim", "buero", Some(VlanSet::from_range(20, 30)));
    texts.network = render_network(&network);
    reply(&mut app, Request::GetConfig, Reply::Config(texts));
    key(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(render(&app));
}

fn render_size(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|f| draw(f, app)).expect("draw");
    terminal.backend().to_string()
}

#[test]
fn the_help_bar_always_offers_help_and_quit() {
    let mut app = connected();
    if let Some(d) = &mut app.draft {
        // A modified draft adds a and x to every bar.
        d.network.sites[2].uplink = Some("heim".into());
        d.revalidate();
    }
    for view in ['1', '2', '3', '4', '5', '6', '7'] {
        key(&mut app, KeyCode::Char(view));
        let screen = render(&app);
        let bar = screen.lines().last().unwrap_or_default();
        assert!(
            bar.contains("?  help") && bar.contains("q  quit"),
            "view {view}: {bar}"
        );
        if view != '6' {
            assert!(bar.contains("a  apply"), "view {view}: {bar}");
        }
    }
}

#[test]
fn a_long_message_gets_two_lines() {
    let mut app = connected();
    app.say(
        MessageKind::Error,
        "this site (\"haus5\") may not change site \"buero\": its VLANs, mappings and online state are managed by only \"heim\"; make the change there",
    );
    insta::assert_snapshot!(render(&app));
}

#[test]
fn messages_window() {
    let mut app = connected();
    app.say(MessageKind::Info, "first message");
    app.say(MessageKind::Success, "second message");
    app.say(MessageKind::Error, "third message");
    key(&mut app, KeyCode::Char('M'));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn terminal_too_small() {
    insta::assert_snapshot!(render_size(&connected(), 60, 20));
}
