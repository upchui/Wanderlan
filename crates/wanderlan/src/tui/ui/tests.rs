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
use crate::tui::app::{App, MessageKind, Modal, Msg, Reply};
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
        d.local
            .tunnel_trunks
            .insert("haus2".into(), vec!["ens20".into()]);
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

/// The fixture network with six more clients of heim.
fn crowded() -> App {
    let mut app = connected();
    let mut status = fixtures::status();
    for i in 0..6 {
        let mut s = fixtures::summary(&format!("lab{i}"), &format!("10.8.1.{i}"), "10-50");
        s.uplink = Some("heim".into());
        status.sites.push(s);
    }
    reply(&mut app, Request::Status, Reply::Status(Box::new(status)));
    app
}

#[test]
fn overview_with_many_sites_scrolls() {
    let mut app = crowded();
    // Down through the five peers into the sites, to the last one.
    key(&mut app, KeyCode::End);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn overview_on_a_wide_terminal() {
    insta::assert_snapshot!(render_size(&crowded(), 120, 40));
}

#[test]
fn a_long_form_hint_wraps() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('n'));
    // Down to "Connected to", whose hint is longer than a line.
    for _ in 0..5 {
        key(&mut app, KeyCode::Tab);
    }
    insta::assert_snapshot!(render(&app));
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

#[test]
fn sites_search() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('/'));
    typed(&mut app, "haus 10-50");
    insta::assert_snapshot!(render(&app));
    // Enter keeps the filter; Up/Down move among the rows shown.
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::End);
    let d = app.draft.as_ref().unwrap();
    assert_eq!(d.network.sites[app.sites_selected].name, "haus5");
}

#[test]
fn log_search_in_local_time() {
    let mut app = connected();
    app.log.zone = crate::tui::localtime::from_rule("CET-1CEST,M3.5.0,M10.5.0/3");
    key(&mut app, KeyCode::Char('5'));
    key(&mut app, KeyCode::Char('/'));
    typed(&mut app, "loop");
    key(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(render(&app));
    key(&mut app, KeyCode::Char('t'));
    assert!(render(&app).contains("times in UTC"), "t switches to UTC");
}

#[test]
fn site_details() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(render(&app));
    // Down: the next site of the tree (haus6, the client of heim).
    key(&mut app, KeyCode::Down);
    assert!(render(&app).contains(" haus6 "), "{}", render(&app));
}

#[test]
fn the_mouse_selects_opens_and_scrolls() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = App::new("/run/wanderlan/wanderlan.sock", true);
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
    let mouse = |app: &mut App, kind, column, row| {
        app.update(Msg::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
        render(app);
    };
    key(&mut app, KeyCode::Char('2'));
    render(&app);
    // Row 5 of the screen is the third site row (buero).
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 5);
    let name = |app: &App| {
        app.draft.as_ref().unwrap().network.sites[app.sites_selected]
            .name
            .clone()
    };
    assert_eq!(name(&app), "buero");
    assert!(app.modal.is_none());
    // A click on the selected row opens it.
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 5);
    assert!(matches!(&app.modal, Some(Modal::Site { name }) if name == "buero"));
    key(&mut app, KeyCode::Esc);
    render(&app);
    // The wheel over the list moves the selection.
    mouse(&mut app, MouseEventKind::ScrollDown, 10, 5);
    assert_eq!(name(&app), "haus2");
    // A click on "?" in the help bar opens the help.
    let bar = app.scrolls.bar.borrow().clone();
    let (start, _, _) = bar
        .iter()
        .find(|(_, _, k)| *k == "?")
        .copied()
        .expect("? in the bar");
    mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        start + 1,
        23,
    );
    assert!(matches!(app.modal, Some(Modal::Help { .. })));
}

#[test]
fn several_trunk_ports_in_the_settings() {
    let mut app = connected();
    key(&mut app, KeyCode::Char('7'));
    // The trunk row: the fixed network rows, then "This site", then "Trunk".
    let trunk_row = super::settings::ROWS
        .iter()
        .position(|r| r.key == "trunk")
        .unwrap();
    for _ in 0..trunk_row {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Enter);
    assert!(render(&app).contains("Loading the interfaces"));
    reply(
        &mut app,
        Request::Interfaces,
        Reply::Interfaces(fixtures::interfaces()),
    );
    // A second port typed in "other", then made the main port.
    key(&mut app, KeyCode::End);
    typed(&mut app, "ens20");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Up);
    key(&mut app, KeyCode::Char('m'));
    insta::assert_snapshot!(render(&app));
    key(&mut app, KeyCode::Enter);
    let d = app.draft.as_ref().expect("draft");
    assert_eq!(d.local.trunk, ["ens20", "ens19"]);
    assert!(render(&app).contains("ens20, ens19"));
}

#[test]
fn the_interface_of_the_tunnels_as_a_trunk_port() {
    let mut app = connected();
    reply(
        &mut app,
        Request::Interfaces,
        Reply::Interfaces(fixtures::interfaces()),
    );
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('l'));
    // ens18 carries the tunnel to buero: marked, chosen anyway.
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Char(' '));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn trunk_ports_of_a_tunnel() {
    let mut app = connected();
    reply(
        &mut app,
        Request::Interfaces,
        Reply::Interfaces(fixtures::interfaces()),
    );
    key(&mut app, KeyCode::Char('7'));
    // "to haus2": the fixed rows, then buero, then haus2.
    for _ in 0..=super::settings::ROWS.len() {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Enter);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn peer_details_with_its_vlans() {
    let mut app = connected();
    key(&mut app, KeyCode::Enter);
    reply(
        &mut app,
        Request::VlanTraffic,
        Reply::VlanTraffic(fixtures::vlan_traffic()),
    );
    key(&mut app, KeyCode::Down);
    insta::assert_snapshot!(render(&app));
}

#[test]
fn one_vlan_live() {
    let mut app = connected();
    reply(&mut app, Request::MacTable, Reply::Macs(fixtures::macs()));
    key(&mut app, KeyCode::Enter);
    reply(
        &mut app,
        Request::VlanTraffic,
        Reply::VlanTraffic(fixtures::vlan_traffic()),
    );
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(Modal::Vlan { vid: 20, .. })));
    insta::assert_snapshot!(render(&app));
}

#[test]
fn no_traffic_per_vlan_without_counters() {
    let mut app = connected();
    key(&mut app, KeyCode::Enter);
    let mut traffic = fixtures::vlan_traffic();
    traffic.available = false;
    traffic.note = Some("the kernel has no per-VLAN counters per port".into());
    reply(&mut app, Request::VlanTraffic, Reply::VlanTraffic(traffic));
    assert!(render(&app).contains("No traffic per VLAN"));
}
