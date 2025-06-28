use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();

    let config = romero::Config::build(&args)?;

    romero::run(config)
}
