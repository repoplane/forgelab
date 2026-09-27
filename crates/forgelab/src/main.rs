//! Command forgelab. The full CLI arrives at milestone M4; until then only `version`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("version") | Some("--version") => println!("forgelab {}", forgelab::VERSION),
        _ => {
            eprintln!("forgelab: the CLI is not implemented yet");
            std::process::exit(2);
        }
    }
}
