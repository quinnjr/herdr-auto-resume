mod config;
mod herdr;
mod resume;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        "startup" | "hook-pane" | "supervise-all" | "status" | "stop" | "logs" => {
            eprintln!("auto-resume: '{cmd}' not yet implemented (task 1 skeleton)");
            std::process::exit(0);
        }
        _ => {
            eprintln!("usage: auto-resume <startup|hook-pane|supervise-all|status|stop|logs>");
            std::process::exit(2);
        }
    }
}
