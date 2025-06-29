use quick_xml::Reader;
use quick_xml::events::Event;
use std::error::Error;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

pub struct Rom {
    pub name: String,
    pub sha1: String,
}

pub struct Game {
    pub name: String,
    pub roms: Vec<Rom>,
}

pub struct Dat {
    pub name: String,
    pub games: Vec<Game>,
}

pub fn load_dats(path: &Path) -> Result<Vec<Dat>, Box<dyn Error>> {
    let mut dats = Vec::new();

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

fn parse_dat_file(file_path: &Path) -> Result<Dat, Box<dyn Error>> {
    let file = File::open(file_path)?;
    let buf_reader = BufReader::new(file);
    let mut xml_reader = Reader::from_reader(buf_reader);
    xml_reader.config_mut().trim_text(true);

    let mut dat = Dat {
        name: String::new(),
        games: Vec::new(),
    };

    let mut path_stack: Vec<String> = Vec::new();
    let mut game: Option<Game> = None;

    let mut buf = Vec::new();
    loop {
        match xml_reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let element_name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                path_stack.push(element_name);

                let path = path_stack.join(".");
                if path == "datafile.game" {
                    if let Some(name_attr) = e.attributes().find(|a| {
                        a.as_ref()
                            .map_or(false, |attr| attr.key.as_ref() == b"name")
                    }) {
                        let name = String::from_utf8_lossy(&name_attr.unwrap().value).into_owned();
                        game = Some(Game {
                            name,
                            roms: Vec::new(),
                        });
                    }
                } else if path == "datafile.game.rom" {
                    process_rom_element(e, &mut game);
                }
            }

            Event::Empty(ref e) => {
                let element_name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                path_stack.push(element_name);

                let path = path_stack.join(".");
                if path == "datafile.game.rom" {
                    process_rom_element(e, &mut game);
                }

                path_stack.pop();
            }

            Event::Text(ref e) => {
                let path = path_stack.join(".");
                if path == "datafile.header.name" {
                    dat.name = e.unescape()?.into_owned();
                }
            }

            Event::End(_) => {
                let path = path_stack.join(".");
                if path == "datafile.game" {
                    if let Some(game) = game.take() {
                        dat.games.push(game);
                    }
                }

                path_stack.pop();
            }

            Event::Eof => break,

            _ => {}
        }

        buf.clear();
    }

    Ok(dat)
}

fn process_rom_element(e: &quick_xml::events::BytesStart, game: &mut Option<Game>) {
    if let Some(game) = game {
        let mut rom_name = String::new();
        let mut rom_sha1 = String::new();

        for attr_result in e.attributes() {
            if let Ok(attr) = attr_result {
                match attr.key.as_ref() {
                    b"name" => rom_name = String::from_utf8_lossy(&attr.value).into_owned(),
                    b"sha1" => rom_sha1 = String::from_utf8_lossy(&attr.value).into_owned(),
                    _ => {}
                }
            }
        }

        game.roms.push(Rom {
            name: rom_name,
            sha1: rom_sha1,
        });
    }
}
