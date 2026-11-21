mod completion;
mod highlighter;
mod history_hint;
mod jobs;
mod policy;
mod prompt;
mod repl;
mod sysinfo;
mod terminal;

fn main() {
    epsh::cli::run(Some(repl::run));
}
