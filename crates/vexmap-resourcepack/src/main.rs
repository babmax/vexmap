//! `vexmap-resourcepack <directory>` writes `vexmap-pack.zip` and `vexmap-pack.sha1` there. The hosting of the file is the
//! operator's: region servers take its address and hash from `VEXMAP_PACK_URL` and `VEXMAP_PACK_SHA1`.

use std::path::PathBuf;
use std::process::ExitCode;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(std::env::args().nth(1).ok_or("usage: vexmap-resourcepack <directory>")?);
    std::fs::create_dir_all(&directory)?;
    let pack = vexmap_resourcepack::build()?;
    let sha1 = vexmap_resourcepack::sha1_hex(&pack);
    std::fs::write(directory.join("vexmap-pack.zip"), &pack)?;
    std::fs::write(directory.join("vexmap-pack.sha1"), format!("{sha1}\n"))?;
    println!("vexmap-pack.zip: {} bytes, sha1 {sha1}", pack.len());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vexmap-resourcepack: {error}");
            ExitCode::FAILURE
        }
    }
}
