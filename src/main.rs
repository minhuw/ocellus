use std::env;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let mut args = env::args().skip(1);

    match args.next().as_deref() {
        Some("--help" | "-h") => print_help(),
        Some("--version" | "-V") => println!("ocellus {VERSION}"),
        Some(command) => {
            eprintln!("unknown command or flag: {command}");
            eprintln!("try `ocellus --help`");
            std::process::exit(2);
        }
        None => run(),
    }
}

fn run() {
    println!("ocellus: hardware telemetry exporter skeleton");
}

fn print_help() {
    println!(
        "ocellus {VERSION}\n\nUsage:\n  ocellus [OPTIONS]\n\nOptions:\n  -h, --help     Print help\n  -V, --version  Print version"
    );
}
