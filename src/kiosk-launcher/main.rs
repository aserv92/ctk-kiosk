mod app;
mod config;
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
