mod repl;

fn main() {
    epsh::cli::run(Some(repl::run));
}
