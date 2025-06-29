use std::error::Error;
use std::path::Path;
use std::fs::{self, File};
use std::io::BufReader;
use quick_xml::events::Event;
use quick_xml::Reader;

pub struct Rom {
    pub name: String,
    pub sha1: String
}

pub struct Game {
    pub name: String,
    pub roms: Vec<Rom>
}

pub struct DAT {
    pub name: String,
    pub games: Vec<Game>
}

pub fn load_dats(path: &Path) -> Result<Vec<DAT>, Box<dyn Error>> {
    let mut dats = Vec::new();
    
    // Read all .dat files in the directory
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_path = entry.path();
        
        if file_path.extension().and_then(|s| s.to_str()) == Some("dat") {
            let dat = parse_dat_file(&file_path)?;
            dats.push(dat);
        }
    }
    
    Ok(dats)
}

fn parse_dat_file(file_path: &Path) -> Result<DAT, Box<dyn Error>> {
    let file = File::open(file_path)?;
    let buf_reader = BufReader::new(file);
    let mut reader = Reader::from_reader(buf_reader);
    reader.config_mut().trim_text(true);
    
    let mut dat = DAT {
        name: String::new(),
        games: Vec::new(),
    };
    
    let mut current_game: Option<Game> = None;
    let mut in_header_name = false;
    let mut buf = Vec::new();
    
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                match e.name().as_ref() {
                    b"name" => {
                        // Check if we're in header (no current game means we're in header)
                        if current_game.is_none() {
                            in_header_name = true;
                        }
                    }
                    b"game" => {
                        // Extract game name from attributes
                        if let Some(name_attr) = e.attributes().find(|a| 
                            a.as_ref().map_or(false, |attr| attr.key.as_ref() == b"name")
                        ) {
                            let name = String::from_utf8_lossy(&name_attr.unwrap().value).into_owned();
                            current_game = Some(Game {
                                name,
                                roms: Vec::new(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Event::Empty(ref e) => {
                match e.name().as_ref() {
                    b"rom" => {
                        if let Some(ref mut game) = current_game {
                            let mut rom_name = String::new();
                            let mut rom_sha1 = String::new();
                            
                            // Parse attributes in one pass
                            for attr_result in e.attributes() {
                                if let Ok(attr) = attr_result {
                                    match attr.key.as_ref() {
                                        b"name" => rom_name = String::from_utf8_lossy(&attr.value).into_owned(),
                                        b"sha1" => rom_sha1 = String::from_utf8_lossy(&attr.value).into_owned(),
                                        _ => {} // Skip other attributes
                                    }
                                }
                            }
                            
                            game.roms.push(Rom {
                                name: rom_name,
                                sha1: rom_sha1,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(ref e) => {
                if in_header_name {
                    dat.name = e.unescape()?.into_owned();
                }
            }
            Event::End(ref e) => {
                match e.name().as_ref() {
                    b"name" => {
                        in_header_name = false;
                    }
                    b"game" => {
                        if let Some(game) = current_game.take() {
                            dat.games.push(game);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {} // Skip all other events
        }
        buf.clear();
    }
    
    Ok(dat)
}
