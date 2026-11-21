// Manual launcher: `cargo run -p opencode-tui --example tui -- [--smoke] [-m provider/model] [--agent name]
// [-s session] [-c] [--prompt text] [directory]`.
use opencode_tui::TuiOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut options = TuiOptions { directory: std::env::current_dir()?, ..Default::default() };
    let mut smoke = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--smoke" => smoke = true,
            "-m" | "--model" => options.model = args.next(),
            "--agent" => options.agent = args.next(),
            "-s" | "--session" => options.session = args.next(),
            "-c" | "--continue" => options.continue_last = true,
            "--prompt" => options.prompt = args.next(),
            directory => options.directory = directory.into(),
        }
    }
    if smoke {
        opencode_tui::smoke(options).await?;
        println!("smoke ok");
        return Ok(());
    }
    opencode_tui::run(options).await
}
