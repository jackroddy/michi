use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Run a .michi pipeline file
#[derive(Parser)]
#[command(name = "michi", version)]
struct Cli {
    /// The .michi file
    file: PathBuf,

    /// Print the plan, with the cores each command would pin, and run nothing
    #[arg(long)]
    dry_run: bool,

    /// Print the file as a bash script and run nothing
    #[arg(long, conflicts_with = "dry_run")]
    sh: bool,

    /// Run only this pipeline; repeat for several, in the order given
    #[arg(short, long = "pipeline", value_name = "NAME")]
    pipelines: Vec<String>,

    /// No progress output, the file's own included
    #[arg(long)]
    silent: bool,

    /// Write a table of the run here; an error when the file declares one
    #[arg(long, value_name = "PATH")]
    table: Option<PathBuf>,
}

// exit codes: 0 when every step passed, 1 when a step or a pipeline
// failed while running, 2 when nothing ran at all, for a file that
// would not load, a pipeline this machine cannot build, or a bad
// command line, which clap reports with 2 itself
fn main() -> ExitCode {
    let cli = Cli::parse();
    let path = cli.file.display().to_string();
    let source = match std::fs::read_to_string(&cli.file) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{path}: {err}");
            return ExitCode::from(2);
        }
    };
    let name = cli
        .file
        .file_name()
        .map_or(path.clone(), |n| n.to_string_lossy().into_owned());
    let file = match michi_cli::load(&source, &cli.pipelines) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("{}", err.render(&path, &source));
            return ExitCode::from(2);
        }
    };
    if cli.sh {
        print!("{}", michi_cli::script(&file, &name));
        return ExitCode::SUCCESS;
    }
    let sinks = michi_cli::Sinks {
        progress: !cli.silent,
        table: cli.table,
    };
    let built = match michi_cli::build(&file, &sinks) {
        Ok(built) => built,
        Err(err) => {
            eprintln!("{}", err.render(&path, &source));
            return ExitCode::from(2);
        }
    };
    if cli.dry_run {
        for built in built {
            println!("# pipeline {}", built.name);
            built.pipeline.dry_run();
        }
        return ExitCode::SUCCESS;
    }
    for built in built {
        if let Err(err) = built.pipeline.run() {
            eprintln!("{path}: pipeline {}: {err}", built.name);
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
