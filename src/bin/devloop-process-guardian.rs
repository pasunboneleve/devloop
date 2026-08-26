use anyhow::Result;

fn main() -> Result<()> {
    devloop::process_guardian::run_and_exit(std::env::args_os().skip(1).collect())
}
