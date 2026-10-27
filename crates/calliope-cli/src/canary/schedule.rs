use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};

const FAULT_SCHEDULE: &str = include_str!("../../../../deploy/canary/fault-schedule.txt");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    Netem,
    // a netem rate cap at a multiple of the traffic measured in the clean window before
    Cap,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub kind: FaultKind,
    pub name: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub secs: u64,
    // none for a clean window
    pub fault: Option<Fault>,
}

#[derive(Debug)]
pub struct FaultSchedule {
    pub windows: Vec<Window>,
}

// the clean window after a fault, in unix seconds, in which every faulted row must recover
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryWindow {
    pub fault: Fault,
    pub start: u64,
    pub end: u64,
}

pub fn fault_schedule() -> FaultSchedule {
    FaultSchedule::parse(FAULT_SCHEDULE).expect("deploy/canary/fault-schedule.txt")
}

impl FaultSchedule {
    pub fn parse(text: &'static str) -> Result<Self> {
        let mut windows = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let window = parse_window(line).with_context(|| format!("line {}", index + 1))?;
            windows.push(window);
        }
        ensure!(!windows.is_empty(), "no windows");
        for (index, window) in windows.iter().enumerate() {
            let next = windows[(index + 1) % windows.len()];
            ensure!(
                window.fault.is_some() != next.fault.is_some(),
                "window {} and the one after it are both {}, faults and clean windows alternate",
                index + 1,
                if next.fault.is_some() {
                    "faults"
                } else {
                    "clean"
                }
            );
        }
        let mut names = HashSet::new();
        for fault in windows.iter().filter_map(|window| window.fault) {
            ensure!(
                names.insert(fault.name),
                "fault '{}' named twice",
                fault.name
            );
        }
        Ok(FaultSchedule { windows })
    }

    pub fn cycle_secs(&self) -> u64 {
        self.windows.iter().map(|window| window.secs).sum()
    }

    pub fn faults(&self) -> impl Iterator<Item = Fault> + '_ {
        self.windows.iter().filter_map(|window| window.fault)
    }

    // the longest a faulted row can go without a frame and still recover: a fault and the clean window after it
    pub fn longest_fault_and_recovery_secs(&self) -> u64 {
        self.windows
            .iter()
            .zip(self.windows.iter().cycle().skip(1))
            .filter(|(window, _)| window.fault.is_some())
            .map(|(fault, clean)| fault.secs + clean.secs)
            .max()
            .expect("a schedule has a fault")
    }

    // the first recovery window ending after `now`, with windows anchored at multiples of the cycle since the unix epoch
    pub fn next_recovery_window(&self, now: u64) -> RecoveryWindow {
        let mut start = now - now % self.cycle_secs();
        // the first window comes after the last one of the previous cycle
        let mut previous_fault = self.windows.last().and_then(|window| window.fault);
        let mut index = 0;
        loop {
            let window = self.windows[index];
            let end = start + window.secs;
            match window.fault {
                Some(fault) => previous_fault = Some(fault),
                None if end > now => {
                    return RecoveryWindow {
                        fault: previous_fault.expect("faults and clean windows alternate"),
                        start,
                        end,
                    };
                }
                None => {}
            }
            start = end;
            index = (index + 1) % self.windows.len();
        }
    }
}

// `<kind> <secs> <arguments>: <fault name>`, a clean window has no arguments and no name
fn parse_window(line: &'static str) -> Result<Window> {
    let (spec, name) = match line.split_once(':') {
        Some((spec, name)) => (spec, Some(name.trim())),
        None => (line, None),
    };
    let mut fields = spec.split_whitespace();
    let kind = fields.next().context("empty window")?;
    let secs: u64 = fields
        .next()
        .context("no seconds")?
        .parse()
        .context("seconds")?;
    ensure!(secs > 0, "a window lasts at least a second");
    let arguments: Vec<&str> = fields.collect();
    let kind = match kind {
        "clean" => {
            ensure!(
                arguments.is_empty() && name.is_none(),
                "a clean window takes no arguments and no name"
            );
            return Ok(Window { secs, fault: None });
        }
        "netem" => {
            ensure!(!arguments.is_empty(), "netem needs its arguments");
            FaultKind::Netem
        }
        "cap" => {
            let multiple: Option<f64> = match arguments[..] {
                [multiple] => multiple.parse().ok(),
                _ => None,
            };
            ensure!(
                multiple.is_some_and(|multiple| multiple > 0.0),
                "cap takes one positive multiple of the measured traffic"
            );
            FaultKind::Cap
        }
        "restart" => {
            ensure!(arguments.is_empty(), "restart takes no arguments");
            FaultKind::Restart
        }
        other => bail!("unknown window kind '{other}'"),
    };
    let name = name
        .filter(|name| !name.is_empty())
        .context("a fault needs a name")?;
    Ok(Window {
        secs,
        fault: Some(Fault { kind, name }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEDULE: &str = "\
# comment
clean 120
netem 60 loss 3%: 3% packet loss
clean 60
cap 60 0.5: half the traffic
clean 90
restart 1: server restart
";

    #[test]
    fn the_deployed_schedule_parses() {
        let schedule = fault_schedule();
        assert!(schedule.faults().count() > 0);
    }

    #[test]
    fn parses_kinds_and_names() {
        let schedule = FaultSchedule::parse(SCHEDULE).unwrap();
        assert_eq!(schedule.cycle_secs(), 391);
        // the half traffic cap and the 90 s clean window after it
        assert_eq!(schedule.longest_fault_and_recovery_secs(), 150);
        let faults: Vec<Fault> = schedule.faults().collect();
        assert_eq!(
            faults,
            vec![
                Fault {
                    kind: FaultKind::Netem,
                    name: "3% packet loss"
                },
                Fault {
                    kind: FaultKind::Cap,
                    name: "half the traffic"
                },
                Fault {
                    kind: FaultKind::Restart,
                    name: "server restart"
                },
            ]
        );
    }

    #[test]
    fn rejects_schedules_the_recovery_check_cannot_judge() {
        let two_cleans = SCHEDULE.replace("netem 60 loss 3%: 3% packet loss\n", "");
        assert!(FaultSchedule::parse(two_cleans.leak()).is_err());
        let fault_last_then_fault_first = "netem 60 loss 3%: a\nclean 60\nrestart 1: b\n";
        assert!(FaultSchedule::parse(fault_last_then_fault_first).is_err());
        let same_name = SCHEDULE.replace("half the traffic", "3% packet loss");
        assert!(FaultSchedule::parse(same_name.leak()).is_err());
        let unnamed = SCHEDULE.replace(": server restart", "");
        assert!(FaultSchedule::parse(unnamed.leak()).is_err());
        let bad_cap = SCHEDULE.replace("cap 60 0.5", "cap 60 half");
        assert!(FaultSchedule::parse(bad_cap.leak()).is_err());
        let named_clean = SCHEDULE.replace("clean 60", "clean 60: quiet");
        assert!(FaultSchedule::parse(named_clean.leak()).is_err());
    }

    #[test]
    fn recovery_windows_are_anchored_to_the_clock() {
        let schedule = FaultSchedule::parse(SCHEDULE).unwrap();
        let cycle = schedule.cycle_secs();
        let cycle_start = 1_000 * cycle;
        // the first clean window follows the last fault of the previous cycle
        let after_restart = schedule.next_recovery_window(cycle_start);
        assert_eq!(after_restart.fault.name, "server restart");
        assert_eq!(
            (after_restart.start, after_restart.end),
            (cycle_start, cycle_start + 120)
        );
        // during the loss window, the next judgement is the clean window after it
        let after_loss = schedule.next_recovery_window(cycle_start + 150);
        assert_eq!(after_loss.fault.name, "3% packet loss");
        assert_eq!(
            (after_loss.start, after_loss.end),
            (cycle_start + 180, cycle_start + 240)
        );
        // a window's end belongs to the next one, so judging at the end moves on
        assert_eq!(
            schedule.next_recovery_window(after_loss.end).fault.name,
            "half the traffic"
        );
        // past the last clean window, the next one is in the following cycle
        let wrapped = schedule.next_recovery_window(cycle_start + cycle - 1);
        assert_eq!(wrapped.start, cycle_start + cycle);
    }
}
