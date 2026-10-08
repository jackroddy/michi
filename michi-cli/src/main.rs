use std::process::ExitCode;

// `michi FILE` runs every pipeline in the file in order, stopping at
// the first that fails, and `michi sh FILE` prints the script. both
// are provisional, until the command line is designed
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (want_script, path) = match args.as_slice() {
        [cmd, path] if cmd == "sh" => (true, path),
        [path] => (false, path),
        _ => {
            eprintln!("usage: michi [sh] <file.michi>");
            return ExitCode::from(2);
        }
    };
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{path}: {err}");
            return ExitCode::from(2);
        }
    };
    let name = std::path::Path::new(path)
        .file_name()
        .map_or(path.clone(), |n| n.to_string_lossy().into_owned());
    if want_script {
        return match michi_cli::compile(&source, &name) {
            Ok(script) => {
                print!("{script}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("{}", err.render(path, &source));
                ExitCode::FAILURE
            }
        };
    }
    let built = match michi_cli::build(&source) {
        Ok(built) => built,
        Err(err) => {
            eprintln!("{}", err.render(path, &source));
            return ExitCode::FAILURE;
        }
    };
    for built in built {
        if let Err(err) = built.pipeline.run() {
            eprintln!("{}: pipeline {}: {err}", path, built.name);
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
