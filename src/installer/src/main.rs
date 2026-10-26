mod disks;
mod install;
mod ui;

use std::process::Command;

fn main() {
    let code = match ui::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    };
    if code == 0 {
        let _ = Command::new("reboot").status();
    }
    std::process::exit(code);
}
