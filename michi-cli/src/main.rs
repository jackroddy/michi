use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: michi <file.michi>");
        return ExitCode::from(2);
    };
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{path}: {err}");
            return ExitCode::from(2);
        }
    };
    match michi_cli::parse(&source) {
        Ok(file) => {
            let pipelines = file
                .items
                .iter()
                .filter(|item| matches!(item, michi_cli::ast::Item::Pipeline(_)))
                .count();
            println!("{path}: {pipelines} pipeline(s)");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{}", err.render(&path, &source));
            ExitCode::FAILURE
        }
    }
}
