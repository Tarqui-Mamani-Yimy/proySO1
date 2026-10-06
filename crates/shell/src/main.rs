mod repl;
mod terminal;

fn main() {
    epsh::cli::run(Some(repl::run));
}
