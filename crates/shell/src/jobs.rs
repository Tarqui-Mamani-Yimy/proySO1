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
    terminal_modes: Option<libc::termios>,
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
            terminal_modes: None,
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
    let foreground_jobs = jobs.clone();
    shell.register_embedder_builtin("fg", move |shell, argv| {
        let args = argv
            .iter()
            .skip(1)
            .map(ShellBytes::to_shell_string)
            .collect::<Vec<_>>();
        Ok(foreground(shell, &foreground_jobs, &args))
    })?;
    let background_jobs = jobs.clone();
    shell.register_embedder_builtin("bg", move |shell, argv| {
        let args = argv
            .iter()
            .skip(1)
            .map(ShellBytes::to_shell_string)
            .collect::<Vec<_>>();
        Ok(background(shell, &background_jobs, &args))
    })?;
    Ok(jobs)
}

fn select_job(
    table: &JobTable,
    args: &[String],
) -> Result<(usize, i32, String, JobState), &'static str> {
    if args.len() > 1 {
        return Err("usage: expected at most one job specification");
    }
    let spec = args.first().map(String::as_str).unwrap_or("%+");
    let number = table.resolve(spec)?;
    let job = table
        .jobs
        .iter()
        .find(|job| job.number == number)
        .ok_or("no such job")?;
    Ok((job.number, job.pgid, job.command.clone(), job.state))
}

fn background(shell: &mut Shell, table: &Arc<Mutex<JobTable>>, args: &[String]) -> ExitStatus {
    let selected = table.lock().unwrap_or_else(|p| p.into_inner());
    let (number, pgid, command, state) = match select_job(&selected, args) {
        Ok(job) => job,
        Err(error) => {
            shell.write_stderr(format!("bg: {error}\n").as_bytes());
            return ExitStatus::FAILURE;
        }
    };
    drop(selected);
    if !matches!(state, JobState::Running | JobState::Stopped)
        || shell.signal_owned_job(pgid, libc::SIGCONT).is_err()
    {
        shell.write_stderr(b"bg: job cannot be continued\n");
        return ExitStatus::FAILURE;
    }
    sync(shell, table);
    shell.write_stdout(format!("[{number}]+ {command} &\n").as_bytes());
    ExitStatus::SUCCESS
}

fn foreground(shell: &mut Shell, table: &Arc<Mutex<JobTable>>, args: &[String]) -> ExitStatus {
    let selected = table.lock().unwrap_or_else(|p| p.into_inner());
    let (number, pgid, command, state) = match select_job(&selected, args) {
        Ok(job) => job,
        Err(error) => {
            shell.write_stderr(format!("fg: {error}\n").as_bytes());
            return ExitStatus::FAILURE;
        }
    };
    drop(selected);
    let tty = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(tty) => tty,
        Err(_) => {
            shell.write_stderr(b"fg: no job control\n");
            return ExitStatus::FAILURE;
        }
    };
    use std::os::fd::AsRawFd;
    let fd = tty.as_raw_fd();
    let shell_group = unsafe { libc::getpgrp() };
    let mut shell_modes = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetpgrp(fd) } != shell_group
        || unsafe { libc::tcgetattr(fd, &mut shell_modes) } == -1
    {
        shell.write_stderr(b"fg: no job control\n");
        return ExitStatus::FAILURE;
    }
    let mut job_modes = table
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .jobs
        .iter()
        .find(|job| job.number == number)
        .and_then(|job| job.terminal_modes);
    unsafe {
        libc::tcsetpgrp(fd, pgid);
    }
    if let Some(modes) = job_modes.as_ref() {
        unsafe {
            libc::tcsetattr(fd, libc::TCSADRAIN, modes);
        }
    }
    if state == JobState::Stopped && shell.signal_owned_job(pgid, libc::SIGCONT).is_err() {
        unsafe {
            libc::tcsetpgrp(fd, shell_group);
            libc::tcsetattr(fd, libc::TCSADRAIN, &shell_modes);
        }
        shell.write_stderr(b"fg: job cannot be continued\n");
        return ExitStatus::FAILURE;
    }
    loop {
        for update in shell.poll_child_jobs() {
            table
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .apply(update);
        }
        let current = table
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .jobs
            .iter()
            .find(|job| job.number == number)
            .map(|job| job.state);
        if !matches!(current, Some(JobState::Running)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let current = table
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .jobs
        .iter()
        .find(|job| job.number == number)
        .map(|job| job.state);
    if current == Some(JobState::Stopped) {
        let mut modes = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut modes) } == 0 {
            job_modes = Some(modes);
        }
    }
    unsafe {
        libc::tcsetpgrp(fd, shell_group);
        libc::tcsetattr(fd, libc::TCSADRAIN, &shell_modes);
    }
    if current == Some(JobState::Stopped) {
        if let Some(job) = table
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .jobs
            .iter_mut()
            .find(|job| job.number == number)
        {
            job.terminal_modes = job_modes;
        }
        shell.write_stdout(format!("[{number}]+  Stopped  {command}\n").as_bytes());
        return ExitStatus::from(128 + libc::SIGTSTP);
    }
    match current {
        Some(JobState::Done(status)) => status.into(),
        Some(JobState::Killed(signal)) => (128 + signal).into(),
        _ => ExitStatus::FAILURE,
    }
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
    let mut reported = Vec::new();
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
        if matches!(job.state, JobState::Done(_) | JobState::Killed(_)) {
            reported.push(job.number);
        }
    }
    table.jobs.retain(|job| !reported.contains(&job.number));
    table.update_current_previous();
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
