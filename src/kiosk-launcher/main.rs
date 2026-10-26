mod app;
mod config;
mod screensaver;
mod sound;
mod ui;
mod wifi;

fn main() {
    let code = match ui::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    };
    std::process::exit(code);
}
