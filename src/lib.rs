use std::error::Error;
use std::path::Path;

mod dat;

// --dat-dir
// --work-dir
// --archive-dir

pub struct Config {
    dat_path: String,
    work_path: String,
    archive_path: String,
}

impl Config {
    pub fn build(args: &[String]) -> Result<Config, &'static str> {
        // FIXME: properly handle command line arguments
        if args.len() != 4 {
            eprintln!(
                "Usage: {} <dat_dir> <work_dir> <archive_dir>",
                Path::new(&args[0])
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&args[0])
            );
            return Err("invalid number of arguments".into());
        }

        let dat_path = args[1].clone();
        let work_path = args[2].clone();
        let archive_path = args[3].clone();

        Ok(Config {
            dat_path,
            work_path,
            archive_path,
        })
    }
}

fn validate_directory(path: &Path) -> Result<(), Box<dyn Error>> {
    if !path.exists() {
        return Err(format!("directory does not exist: {}", path.display()).into());
    }
    if !path.is_dir() {
        return Err(format!("path is not a directory: {}", path.display()).into());
    }
    Ok(())
}

pub fn run(config: Config) -> Result<(), Box<dyn Error>> {
    let dat_path = Path::new(&config.dat_path);
    let work_path = Path::new(&config.work_path);
    let archive_path = Path::new(&config.archive_path);

    println!("dat directory: {}", dat_path.display());
    println!("work directory: {}", work_path.display());
    println!("archive directory: {}", archive_path.display());

    validate_directory(dat_path)?;
    validate_directory(work_path)?;
    validate_directory(archive_path)?;

    let dats = dat::load_dats(dat_path)?;

    println!("Loaded {} DAT files", dats.len());
    for dat in &dats {
        println!("DAT: {} with {} games", dat.name, dat.games.len());
    }

    println!("Hello, world!");

    Ok(())
}
