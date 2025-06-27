// --dat-dir
// --work-dir
// --archive-dir

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // FIXME: properly handle command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <dat_dir> <work_dir> <archive_dir>", std::path::Path::new(&args[0])
            .file_name().and_then(|name| name.to_str()).unwrap_or(&args[0]));
        return Err("invalid number of arguments".into());
    }
    let dat_path = std::path::Path::new(&args[1]);
    let work_path = std::path::Path::new(&args[2]);
    let archive_dir = std::path::Path::new(&args[3]);

    println!("dat directory: {}", dat_path.display());
    println!("work directory: {}", work_path.display());
    println!("archive directory: {}", archive_dir.display());
    
    validate_directory(dat_path)?;
    validate_directory(work_path)?;
    validate_directory(dat_path)?;

    println!("Hello, world!");

    Ok(())
}

fn validate_directory(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("directory does not exist: {}", path.display()).into());
    }
    if !path.is_dir() {
        return Err(format!("path is not a directory: {}", path.display()).into());
    }
    Ok(())
}
