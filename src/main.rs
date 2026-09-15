mod config;
mod herdr;
mod monitor;
mod resume;
mod state;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        // `monitor <pane-id>` carries the pane id as an argv token: the
        // liveness check in state.rs matches it against /proc cmdlines.
        "monitor" => {
            match args.get(1) {
                Some(id) => monitor::run(id),
                None => {
                    eprintln!("usage: auto-resume monitor <pane-id>");
                    std::process::exit(2);
                }
            }
        }
        "startup" | "hook-pane" | "supervise-all" | "status" | "stop" | "logs" => {
            eprintln!("auto-resume: '{cmd}' not yet implemented (task 1 skeleton)");
            std::process::exit(0);
        }
        _ => {
            eprintln!("usage: auto-resume <startup|hook-pane|supervise-all|status|stop|logs|monitor>");
            std::process::exit(2);
        }
    }
}
