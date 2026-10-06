use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use epsh::error::ExitStatus;
use epsh::eval::{ChildJobInfo, ChildJobState, ChildJobStatus, ChildJobUpdate, Shell};
use epsh::shell_bytes::ShellBytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Running,
    Stopped,
    Done(i32),
    Killed(i32),
}

#[derive(Debug)]
struct Job {
    number: usize,
    pgid: i32,
    members: Vec<i32>,
    pending: HashSet<i32>,
    results: HashMap<i32, MemberResult>,
    state: JobState,
    command: String,
    activity: u64,
}

#[derive(Debug, Clone, Copy)]
enum MemberResult {
    Exited(i32),
    Signaled(i32),
}

#[derive(Default, Debug)]
pub(crate) struct JobTable {
    jobs: Vec<Job>,
    current: Option<usize>,
    previous: Option<usize>,
    next_number: usize,
    activity_clock: u64,
}

impl JobTable {
    fn add(&mut self, info: ChildJobInfo) -> usize {
        self.next_number += 1;
        self.activity_clock += 1;
        let number = self.next_number;
        let pending = info.pids.iter().copied().collect();
        self.jobs.push(Job {
            number,
            pgid: info.pgid,
            members: info.pids,
            pending,
            results: HashMap::new(),
            state: match info.state {
                ChildJobState::Running => JobState::Running,
                ChildJobState::Stopped => JobState::Stopped,
            },
            command: info.command,
            activity: self.activity_clock,
        });
        self.update_current_previous();
        number
    }

    fn apply(&mut self, update: ChildJobUpdate) {
        let changes_activity = matches!(
            update.status,
            ChildJobStatus::Stopped(_) | ChildJobStatus::Continued
        );
        if changes_activity {
            self.activity_clock += 1;
        }
        let Some(job) = self.jobs.iter_mut().find(|job| job.pgid == update.pgid) else {
            return;
        };
        match update.status {
            ChildJobStatus::Stopped(_) => job.state = JobState::Stopped,
            ChildJobStatus::Continued => job.state = JobState::Running,
            ChildJobStatus::Exited(status) => {
                job.pending.remove(&update.pid);
                job.results.insert(update.pid, MemberResult::Exited(status));
                if job.pending.is_empty() {
                    job.state = result_state(job);
                }
            }
            ChildJobStatus::Signaled(signal) => {
                job.pending.remove(&update.pid);
                job.results
                    .insert(update.pid, MemberResult::Signaled(signal));
                if job.pending.is_empty() {
                    job.state = result_state(job);
                }
            }
        }
        if changes_activity {
            job.activity = self.activity_clock;
        }
        self.update_current_previous();
    }

    fn update_current_previous(&mut self) {
        let mut active = self
            .jobs
            .iter()
            .filter(|job| matches!(job.state, JobState::Running | JobState::Stopped))
            .map(|job| (job.activity, job.number))
            .collect::<Vec<_>>();
        active.sort_unstable();
        self.current = active.last().map(|(_, number)| *number);
        self.previous = active.iter().rev().nth(1).map(|(_, number)| *number);
    }

    fn resolve(&self, spec: &str) -> Result<usize, &'static str> {
        match spec {
            "%" | "%+" | "%%" => self.current.ok_or("no current job"),
            "%-" => self.previous.ok_or("no previous job"),
            _ if spec.starts_with('%') => {
                let needle = &spec[1..];
                if needle.is_empty() {
                    return self.current.ok_or("no current job");
                }
                if let Some(partial) = needle.strip_prefix('?') {
                    self.resolve_command(partial, true)
                } else if let Ok(number) = needle.parse::<usize>() {
                    self.jobs
                        .iter()
                        .find(|job| job.number == number)
                        .map(|job| job.number)
                        .ok_or("no such job")
                } else {
                    self.resolve_command(needle, false)
                }
            }
            _ => Err("invalid job specification"),
        }
    }

    fn resolve_command(&self, needle: &str, substring: bool) -> Result<usize, &'static str> {
        let matches = self.jobs.iter().filter(|job| {
            if substring {
                job.command.contains(needle)
            } else {
                job.command.starts_with(needle)
            }
        });
        let mut matches = matches.map(|job| job.number);
        let first = matches.next().ok_or("no such job")?;
        if matches.next().is_some() {
            Err("ambiguous job specification")
        } else {
            Ok(first)
        }
    }

    fn selected<'a>(&'a self, specs: &[String]) -> Result<Vec<&'a Job>, &'static str> {
        if specs.is_empty() {
            return Ok(self.jobs.iter().collect());
        }
        let mut selected = Vec::new();
        for spec in specs {
            let number = self.resolve(spec)?;
            if let Some(job) = self.jobs.iter().find(|job| job.number == number)
                && !selected
                    .iter()
                    .any(|selected: &&Job| selected.number == number)
            {
                selected.push(job);
            }
        }
        Ok(selected)
    }
}

fn result_state(job: &Job) -> JobState {
    match job
        .members
        .last()
        .and_then(|pid| job.results.get(pid))
        .copied()
    {
        Some(MemberResult::Exited(status)) => JobState::Done(status),
        Some(MemberResult::Signaled(signal)) => JobState::Killed(signal),
        None => JobState::Done(ExitStatus::FAILURE.code()),
    }
}

pub(crate) fn register(shell: &mut Shell) -> Result<Arc<Mutex<JobTable>>, String> {
    let jobs = Arc::new(Mutex::new(JobTable::default()));
    let builtin_jobs = jobs.clone();
    shell.register_embedder_builtin("jobs", move |shell, argv| {
        let args = argv
            .iter()
            .skip(1)
            .map(ShellBytes::to_shell_string)
            .collect::<Vec<_>>();
        sync(shell, &builtin_jobs);
        let mut table = builtin_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(list(shell, &mut table, &args))
    })?;
    Ok(jobs)
}

pub(crate) fn sync(shell: &mut Shell, table: &Arc<Mutex<JobTable>>) {
    let new_jobs = shell.take_new_child_jobs();
    let updates = shell.poll_child_jobs();
    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for info in new_jobs {
        let state = info.state;
        let pgid = info.pgid;
        let command = info.command.clone();
        let number = table.add(info);
        let notice = match state {
            ChildJobState::Running => format!("[{number}] {pgid}\n"),
            ChildJobState::Stopped => format!("[{number}]+  Stopped  {command}\n"),
        };
        shell.write_stdout(notice.as_bytes());
    }
    for update in updates {
        table.apply(update);
    }
}

fn list(shell: &mut Shell, table: &mut JobTable, args: &[String]) -> ExitStatus {
    let mut long = false;
    let mut specs = Vec::new();
    for arg in args {
        if arg == "-l" {
            long = true;
        } else if arg.starts_with('-') {
            shell.write_stderr(b"jobs: usage: jobs [-l] [job_spec ...]\n");
            return ExitStatus::MISUSE;
        } else {
            specs.push(arg.clone());
        }
    }
    let selected = match table.selected(&specs) {
        Ok(selected) => selected,
        Err(error) => {
            shell.write_stderr(format!("jobs: {error}\n").as_bytes());
            return ExitStatus::MISUSE;
        }
    };
    let current = table.current;
    let previous = table.previous;
    for job in selected {
        let marker = if Some(job.number) == current {
            "+"
        } else if Some(job.number) == previous {
            "-"
        } else {
            " "
        };
        let state = match job.state {
            JobState::Running => "Running".to_owned(),
            JobState::Stopped => "Stopped".to_owned(),
            JobState::Done(status) => format!("Done({status})"),
            JobState::Killed(signal) => format!("Killed({signal})"),
        };
        let line = if long {
            format!(
                "[{}]{marker} {} {state}  {}\n",
                job.number, job.pgid, job.command
            )
        } else {
            format!("[{}]{marker} {state}  {}\n", job.number, job.command)
        };
        shell.write_stdout(line.as_bytes());
    }
    ExitStatus::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pgid: i32, command: &str) -> ChildJobInfo {
        ChildJobInfo {
            pgid,
            pids: vec![pgid],
            command: command.to_owned(),
            state: ChildJobState::Running,
        }
    }

    #[test]
    fn resolves_all_posix_job_spec_forms() {
        let mut table = JobTable::default();
        table.add(info(101, "sleep 100"));
        table.add(info(102, "grep needle file"));

        assert_eq!(table.resolve("%1"), Ok(1));
        assert_eq!(table.resolve("%+"), Ok(2));
        assert_eq!(table.resolve("%%"), Ok(2));
        assert_eq!(table.resolve("%-"), Ok(1));
        assert_eq!(table.resolve("%sleep"), Ok(1));
        assert_eq!(table.resolve("%?needle"), Ok(2));
    }

    #[test]
    fn current_and_previous_skip_completed_jobs() {
        let mut table = JobTable::default();
        table.add(info(101, "sleep 100"));
        table.add(info(102, "sleep 200"));
        table.apply(ChildJobUpdate {
            pgid: 102,
            pid: 102,
            status: ChildJobStatus::Exited(0),
        });

        assert_eq!(table.resolve("%+"), Ok(1));
        assert_eq!(table.resolve("%-"), Err("no previous job"));
    }

    #[test]
    fn newly_stopped_job_becomes_current() {
        let mut table = JobTable::default();
        table.add(info(101, "sleep 100"));
        table.add(info(102, "sleep 200"));
        table.apply(ChildJobUpdate {
            pgid: 101,
            pid: 101,
            status: ChildJobStatus::Stopped(libc::SIGTSTP),
        });

        assert_eq!(table.resolve("%+"), Ok(1));
        assert_eq!(table.resolve("%-"), Ok(2));
    }
}
