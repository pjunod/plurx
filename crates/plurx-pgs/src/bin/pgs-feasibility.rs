use plurx_pgs::{inspect_sup, ParserLimits};
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: pgs-feasibility FILE.sup");
        std::process::exit(2);
    };
    if args.next().is_some() {
        eprintln!("usage: pgs-feasibility FILE.sup");
        std::process::exit(2);
    }

    match inspect_sup(PathBuf::from(path), &ParserLimits::default()) {
        Ok(report) => match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("could not encode the feasibility report: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!("PGS feasibility check failed: {error}");
            std::process::exit(1);
        }
    }
}
