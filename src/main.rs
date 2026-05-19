mod core;
mod tui;

use std::{env, io, path::PathBuf};

use core::load_result;

enum Command {
    Start,
    Open(PathBuf),
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_usage();
        return Ok(());
    }

    let command = parse_command(args)?;
    let initial_result = match command {
        Command::Start => None,
        Command::Open(path) => Some(load_result(&path).await?),
    };

    tui::run(initial_result)
}

fn parse_command(args: Vec<String>) -> io::Result<Command> {
    if args.is_empty() {
        return Ok(Command::Start);
    }

    if args[0] == "open" {
        let rest: Vec<String> = args.into_iter().skip(1).collect();
        if rest.len() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "open requires exactly one file path",
            ));
        }
        return Ok(Command::Open(PathBuf::from(&rest[0])));
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "unknown command. use --help",
    ))
}

fn print_usage() {
    println!(
        "Disk Analyzer\n\
         Usage:\n\
           diskanalyzer\n\
           diskanalyzer open RESULT.json\n\
           diskanalyzer --help\n\n\
         TUI-first flow:\n\
           s: scan current directory\n\
           a: scan current disk (full)\n\
           p: save to ./diskanalyzer-result.json\n\
           o: load from ./diskanalyzer-result.json\n\
           q: quit"
    );
}
