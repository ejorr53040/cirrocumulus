//! The live dashboard: a Node header with sparklines, a table of running
//! VMs, and a detail pane for the selected one.

use crate::{bytes, duration, per_sec, rate_cells};
use cirro_proto::{Stats, VmStats};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Row, Sparkline, Table, TableState};

/// What the CLI does after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    /// Fetch this VM's console log and pass it to [`Dashboard::set_log`].
    ShowLog(String),
    /// Stop this VM (the user has confirmed).
    Stop(String),
}

/// What `cirro top` shows, and where the user is in it. The CLI owns the
/// terminal and the agent's socket; this owns everything drawn.
#[derive(Default)]
pub struct Dashboard {
    stats: Stats,
    /// The VM the detail pane shows, by name, so it stays selected when a
    /// refresh or a sort moves its row.
    selected: Option<String>,
    sort: Sort,
    /// A VM's console log, shown in the detail pane while it's selected.
    log: Option<(String, String)>,
    /// The VM `k` asked to stop, until `y` confirms or another key cancels.
    confirming_stop: Option<String>,
    /// A one-off message in place of the key help.
    status: Option<String>,
}

/// The column the VM table is sorted by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Sort {
    /// Busiest first, as `top` does.
    #[default]
    Cpu,
    /// Largest first.
    Memory,
    /// A to Z.
    Name,
}

impl Sort {
    fn next(self) -> Sort {
        match self {
            Sort::Cpu => Sort::Memory,
            Sort::Memory => Sort::Name,
            Sort::Name => Sort::Cpu,
        }
    }
}

impl Dashboard {
    /// Shows `stats`. The selected VM stays selected; if it has gone, the
    /// first row is.
    pub fn set_stats(&mut self, stats: Stats) {
        self.stats = stats;
        self.sort_vms();
        if self.selected_row().is_none() {
            self.selected = self.stats.vms.first().map(|vm| vm.info.name.clone());
        }
    }

    fn sort_vms(&mut self) {
        let latest = |vm: &VmStats| vm.history.last().copied().unwrap_or_default();
        match self.sort {
            Sort::Cpu => self
                .stats
                .vms
                .sort_by(|a, b| latest(b).cpu_percent.total_cmp(&latest(a).cpu_percent)),
            Sort::Memory => self
                .stats
                .vms
                .sort_by_key(|vm| std::cmp::Reverse(latest(vm).memory_bytes)),
            Sort::Name => self.stats.vms.sort_by(|a, b| a.info.name.cmp(&b.info.name)),
        }
    }

    /// Shows the end of `log` for VM `name` in the detail pane.
    pub fn set_log(&mut self, name: &str, log: &str) {
        self.log = Some((name.to_string(), log.to_string()));
    }

    /// Shows `message` in place of the key help until the next key.
    pub fn set_status(&mut self, message: String) {
        self.status = Some(message);
    }

    /// Brings the key help back, as when a refresh that failed recovers.
    pub fn clear_status(&mut self) {
        self.status = None;
    }

    /// Handles a key press, and says what the CLI should do about it.
    pub fn key(&mut self, key: KeyCode) -> Action {
        self.status = None;
        if let Some(name) = self.confirming_stop.take() {
            return match key {
                KeyCode::Char('y') => Action::Stop(name),
                _ => Action::None,
            };
        }
        let selected = self.selected().map(|vm| vm.info.name.clone());
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('l') => match (&self.log, selected) {
                // `l` again hides the log.
                (Some((shown, _)), Some(name)) if *shown == name => self.log = None,
                (_, Some(name)) => return Action::ShowLog(name),
                (_, None) => {}
            },
            KeyCode::Char('k') => self.confirming_stop = selected,
            KeyCode::Char('s' | 'p' | 'w') => {
                self.status = Some("shell, park and wake are not yet implemented".into());
            }
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Tab => {
                self.sort = self.sort.next();
                self.sort_vms();
            }
            _ => {}
        }
        Action::None
    }

    /// Moves the selection `by` rows, stopping at the first and last VM.
    fn move_selection(&mut self, by: isize) {
        let Some(last) = self.stats.vms.len().checked_sub(1) else {
            return;
        };
        let row = self.selected_row().unwrap_or(0);
        let row = row.saturating_add_signed(by).min(last);
        self.selected = Some(self.stats.vms[row].info.name.clone());
    }

    /// The selected VM's row, if it's still running.
    fn selected_row(&self) -> Option<usize> {
        let name = self.selected.as_ref()?;
        self.stats.vms.iter().position(|vm| &vm.info.name == name)
    }

    /// Draws everything into `frame`.
    pub fn draw(&self, frame: &mut Frame) {
        let [header, table, detail, help] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(6),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_header(frame, header);
        self.draw_table(frame, table);
        self.draw_detail(frame, detail);
        let footer = match (&self.confirming_stop, &self.status) {
            (Some(name), _) => format!(" stop {name}? y/n"),
            (None, Some(status)) => format!(" {status}"),
            (None, None) => HELP.to_string(),
        };
        frame.render_widget(Paragraph::new(footer), help);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Node ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [text, cpu, memory, net] = Layout::horizontal([
            Constraint::Min(34),
            Constraint::Length(21),
            Constraint::Length(21),
            Constraint::Length(21),
        ])
        .areas(inner);
        let summary = match self.stats.node.last() {
            Some(node) => format!(
                "CPU {:.1}%  MEM {}/{}\nNET {} in, {} out",
                node.cpu_percent,
                bytes(node.memory_used_bytes),
                bytes(node.memory_total_bytes),
                per_sec(node.rx_per_sec),
                per_sec(node.tx_per_sec),
            ),
            None => "sampling…".to_string(),
        };
        frame.render_widget(Paragraph::new(summary), text);
        let node = &self.stats.node;
        let cpu_history: Vec<u64> = node.iter().map(|n| n.cpu_percent.round() as u64).collect();
        let memory_history: Vec<u64> = node.iter().map(|n| n.memory_used_bytes).collect();
        let net_history: Vec<u64> = node.iter().map(|n| n.rx_per_sec + n.tx_per_sec).collect();
        let memory_total = node.last().map(|n| n.memory_total_bytes);
        draw_sparkline(frame, cpu, &cpu_history, " cpu ", Some(100));
        draw_sparkline(frame, memory, &memory_history, " mem ", memory_total);
        draw_sparkline(frame, net, &net_history, " net ", None);
    }

    fn draw_table(&self, frame: &mut Frame, area: Rect) {
        let marked = |label: &str, sort: Sort, mark: &str| {
            if self.sort == sort {
                format!("{label}{mark}")
            } else {
                label.to_string()
            }
        };
        let header = Row::new([
            marked("NAME", Sort::Name, "▲"),
            "VM ADDRESS".to_string(),
            "VCPU".to_string(),
            "UP".to_string(),
            marked("CPU", Sort::Cpu, "▼"),
            marked("MEM", Sort::Memory, "▼"),
            "NET IN".to_string(),
            "NET OUT".to_string(),
            "DISK R".to_string(),
            "DISK W".to_string(),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD));
        let rows = self.stats.vms.iter().map(|vm| {
            let address = vm
                .info
                .vm_address
                .map_or_else(|| "-".to_string(), |a| a.to_string());
            let up = duration(self.stats.now.saturating_sub(vm.info.started_at));
            let [cpu, mem, rx, tx, read, write] = rate_cells(vm.history.last());
            Row::new([
                vm.info.name.clone(),
                address,
                vm.info.vcpus.to_string(),
                up,
                cpu,
                mem,
                rx,
                tx,
                read,
                write,
            ])
        });
        let widths = [
            Constraint::Min(12),
            Constraint::Length(15),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(" VMs "))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default().with_selected(self.selected_row());
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Detail ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(vm) = self.selected() else {
            frame.render_widget(Paragraph::new("no VMs running"), inner);
            return;
        };
        let [text, cpu] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(40)]).areas(inner);
        let lines: Vec<Line> = match &self.log {
            // The end of the log: as many lines as fit.
            Some((name, log)) if *name == vm.info.name => {
                let lines: Vec<&str> = log.lines().collect();
                let fit = usize::from(text.height);
                lines[lines.len().saturating_sub(fit)..]
                    .iter()
                    .map(|l| Line::from(l.to_string()))
                    .collect()
            }
            _ => vec![
                Line::from(vm.info.name.clone()),
                Line::from(format!(
                    "{} vCPU, {} memory",
                    vm.info.vcpus,
                    bytes(u64::from(vm.info.mem_mib) << 20)
                )),
            ],
        };
        frame.render_widget(Paragraph::new(lines), text);
        let history: Vec<u64> = vm
            .history
            .iter()
            .map(|r| r.cpu_percent.round() as u64)
            .collect();
        let ceiling = u64::from(vm.info.vcpus) * 100;
        draw_sparkline(frame, cpu, &history, " cpu ", Some(ceiling));
    }

    fn selected(&self) -> Option<&VmStats> {
        self.stats.vms.get(self.selected_row()?)
    }
}

const HELP: &str = " q quit  ↑↓ select  tab sort  l log  k stop  s shell  p park  w wake";

/// A sparkline of the newest samples in `data` that fit `area` (it holds
/// a minute, oldest first, which is more than most areas are wide), scaled
/// to `max` when there is one.
fn draw_sparkline(frame: &mut Frame, area: Rect, data: &[u64], title: &str, max: Option<u64>) {
    let block = Block::default().borders(Borders::LEFT).title(title);
    let fit = usize::from(block.inner(area).width);
    let newest = &data[data.len().saturating_sub(fit)..];
    let sparkline = Sparkline::default().data(newest).block(block);
    let sparkline = match max {
        Some(max) => sparkline.max(max),
        None => sparkline,
    };
    frame.render_widget(sparkline, area);
}

#[cfg(test)]
mod tests;
