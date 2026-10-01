use super::*;
use cirro_proto::{NodeRates, VmInfo, VmRates, VmStats};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn vm(name: &str, cpu_percent: f64, memory_mib: u64) -> VmStats {
    VmStats {
        info: VmInfo {
            name: name.into(),
            vm_address: Some("10.77.0.2".parse().unwrap()),
            mem_mib: 256,
            vcpus: 1,
            started_at: 0,
            ended: None,
        },
        history: vec![VmRates {
            cpu_percent,
            memory_bytes: memory_mib << 20,
            rx_per_sec: 2048,
            ..Default::default()
        }],
    }
}

fn stats() -> Stats {
    Stats {
        now: 0,
        node: vec![
            NodeRates {
                cpu_percent: 5.0,
                memory_used_bytes: 2 << 30,
                memory_total_bytes: 16 << 30,
                ..Default::default()
            },
            NodeRates {
                cpu_percent: 12.34,
                memory_used_bytes: 3 << 30,
                memory_total_bytes: 16 << 30,
                rx_per_sec: 1536,
                tx_per_sec: 300,
            },
        ],
        vms: vec![vm("web", 99.75, 140), vm("db", 3.0, 600)],
    }
}

/// What a 100x24 terminal shows, one string per row.
fn screen(dashboard: &Dashboard) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| dashboard.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn row<'a>(screen: &'a [String], name: &str) -> Option<&'a str> {
    screen
        .iter()
        .map(String::as_str)
        .find(|line| line.split_whitespace().any(|w| w.trim_matches('│') == name))
}

#[test]
fn it_shows_the_node_and_each_vms_latest_rates() {
    let mut dashboard = Dashboard::default();
    // Every VM in `stats()` started at 0 on the Node's clock.
    dashboard.set_stats(Stats {
        now: 125,
        ..stats()
    });

    let screen = screen(&dashboard);

    let all = screen.join("\n");
    assert!(all.contains("CPU 12.3%"), "{all}");
    for graph in [" cpu ", " mem ", " net "] {
        assert!(screen[1].contains(graph), "no{graph}sparkline:\n{all}");
    }
    assert!(all.contains("MEM 3.0G/16G"), "{all}");
    let web = row(&screen, "web").unwrap_or_else(|| panic!("no web row:\n{all}"));
    let cells: Vec<&str> = web.split_whitespace().collect();
    // vCPUs, then uptime, right after the VM address.
    let at = cells.iter().position(|c| *c == "10.77.0.2").unwrap();
    assert_eq!(cells[at + 1..at + 3], ["1", "2m"], "{web}");
    for cell in ["99.8%", "140M", "2.0K/s"] {
        assert!(web.contains(cell), "web's row lacks {cell}: {web}");
    }
    assert!(row(&screen, "db").is_some(), "no db row:\n{all}");
    assert!(all.contains("q quit"), "no key help:\n{all}");
}

fn detail(screen: &[String]) -> String {
    let start = screen
        .iter()
        .position(|l| l.contains(" Detail "))
        .expect("no detail pane");
    screen[start..].join("\n")
}

#[test]
fn arrow_keys_choose_the_vm_the_detail_pane_shows_and_q_quits() {
    let mut dashboard = Dashboard::default();
    dashboard.set_stats(stats());
    assert!(detail(&screen(&dashboard)).contains("web"));

    assert_eq!(dashboard.key(KeyCode::Down), Action::None);
    assert!(detail(&screen(&dashboard)).contains("db"));
    // The selection stops at the last VM rather than wrapping.
    dashboard.key(KeyCode::Down);
    assert!(detail(&screen(&dashboard)).contains("db"));
    dashboard.key(KeyCode::Up);
    assert!(detail(&screen(&dashboard)).contains("web"));

    assert_eq!(dashboard.key(KeyCode::Char('q')), Action::Quit);
}

/// The VM names in table order.
fn order(screen: &[String]) -> Vec<String> {
    let names = ["web", "db"];
    screen
        .iter()
        .take_while(|l| !l.contains(" Detail "))
        .filter_map(|l| {
            let first = l.split_whitespace().next()?.trim_matches('│');
            names.contains(&first).then(|| first.to_string())
        })
        .collect()
}

#[test]
fn vms_sort_by_cpu_first_and_tab_cycles_memory_then_name() {
    let mut dashboard = Dashboard::default();
    dashboard.set_stats(stats());
    // web uses more CPU, db more memory; by name db comes first.
    assert_eq!(order(&screen(&dashboard)), ["web", "db"]);
    assert!(screen(&dashboard).join("\n").contains("CPU▼"));

    dashboard.key(KeyCode::Tab);
    assert_eq!(order(&screen(&dashboard)), ["db", "web"]);
    assert!(screen(&dashboard).join("\n").contains("MEM▼"));
    // Still web, wherever its row went.
    assert!(detail(&screen(&dashboard)).contains("web"));

    dashboard.set_stats(Stats {
        vms: vec![
            vm("zeta", 50.0, 1),
            vm("web", 99.75, 140),
            vm("db", 3.0, 600),
        ],
        ..stats()
    });
    dashboard.key(KeyCode::Tab);
    assert!(screen(&dashboard).join("\n").contains("NAME▲"));
    let names: Vec<String> = screen(&dashboard)
        .iter()
        .take_while(|l| !l.contains(" Detail "))
        .filter_map(|l| {
            let first = l.split_whitespace().next()?.trim_matches('│');
            ["zeta", "web", "db"]
                .contains(&first)
                .then(|| first.to_string())
        })
        .collect();
    assert_eq!(names, ["db", "web", "zeta"]);

    dashboard.key(KeyCode::Tab);
    assert!(screen(&dashboard).join("\n").contains("CPU▼"));
}

#[test]
fn l_asks_for_the_selected_vms_log_and_shows_its_last_lines() {
    let mut dashboard = Dashboard::default();
    dashboard.set_stats(stats());

    assert_eq!(
        dashboard.key(KeyCode::Char('l')),
        Action::ShowLog("web".into())
    );
    let log: String = (1..=50).map(|n| format!("line {n}\n")).collect();
    dashboard.set_log("web", &log);

    let shown = detail(&screen(&dashboard));
    assert!(shown.contains("line 50"), "{shown}");
    assert!(
        !shown.contains("line 1\n") && !shown.contains("line 1 "),
        "{shown}"
    );

    // `l` again hides it.
    assert_eq!(dashboard.key(KeyCode::Char('l')), Action::None);
    let hidden = detail(&screen(&dashboard));
    assert!(!hidden.contains("line 50"), "{hidden}");
}

#[test]
fn k_stops_the_selected_vm_only_once_confirmed() {
    let mut dashboard = Dashboard::default();
    dashboard.set_stats(stats());

    assert_eq!(dashboard.key(KeyCode::Char('k')), Action::None);
    assert!(screen(&dashboard).join("\n").contains("stop web? y/n"));
    assert_eq!(dashboard.key(KeyCode::Char('n')), Action::None);
    assert!(!screen(&dashboard).join("\n").contains("stop web?"));

    dashboard.key(KeyCode::Char('k'));
    assert_eq!(
        dashboard.key(KeyCode::Char('y')),
        Action::Stop("web".into())
    );
}

#[test]
fn keys_for_what_isnt_built_yet_say_so() {
    let mut dashboard = Dashboard::default();
    dashboard.set_stats(stats());

    for key in ['s', 'p', 'w'] {
        assert_eq!(dashboard.key(KeyCode::Char(key)), Action::None);
        let all = screen(&dashboard).join("\n");
        assert!(all.contains("not yet implemented"), "{key}: {all}");
    }
    // The message lasts until the next key; then the help is back.
    dashboard.key(KeyCode::Down);
    let help = screen(&dashboard).last().unwrap().clone();
    for key in ["q quit", "tab sort", "l log", "k stop"] {
        assert!(help.contains(key), "help lacks {key}: {help}");
    }
}

/// A minute of history is wider than a sparkline: the newest samples are
/// the ones that show.
#[test]
fn sparklines_show_the_newest_samples() {
    let mut stats = stats();
    stats.node = (0..60)
        .map(|second| NodeRates {
            // Idle for the first half minute, flat out for the second.
            cpu_percent: if second < 30 { 0.0 } else { 100.0 },
            memory_total_bytes: 16 << 30,
            ..Default::default()
        })
        .collect();
    let mut dashboard = Dashboard::default();
    dashboard.set_stats(stats);

    let screen = screen(&dashboard);

    // The header's cpu sparkline: its rows, right of the " cpu " title.
    let column = screen[1].find(" cpu ").expect("no cpu sparkline");
    let cells: String = screen[1..3]
        .iter()
        .flat_map(|row| {
            row.chars()
                .skip(screen[1][..column].chars().count())
                .take(20)
        })
        .collect();
    assert!(
        cells.contains('█'),
        "the busy half isn't shown:\n{}",
        screen[..4].join("\n")
    );
}
