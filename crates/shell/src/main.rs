mod completion;
mod highlighter;
mod history_hint;
mod jobs;
mod repl;
mod terminal;

fn main() {
    epsh::cli::run(Some(repl::run));
}
