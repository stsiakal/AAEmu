//! Writes loose files into a COPY of a `game_pak` (never run it on the original client pak).
//!
//! ```text
//! cargo run -p aaemu-commons --example pakwrite -- add <pak copy> <source root> <names.txt>
//! ```
//!
//! Every line of `names.txt` is a pak path (`game/objects/...`); the file is read from
//! `<source root>/<pak path>`. Existing entries are replaced (in place when they fit), new
//! ones are appended. The header is rewritten when the pak is closed.
use aaemu_commons::aapak::AAPak;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, cmd, pak_path, root, names] = args.as_slice() else {
        eprintln!("usage: pakwrite add <pak copy> <source root> <names.txt>");
        std::process::exit(2);
    };
    if cmd != "add" {
        eprintln!("unknown command {cmd}");
        std::process::exit(2);
    }
    let mut pak = AAPak::open(pak_path, false).expect("open pak read-write");
    let names = std::fs::read_to_string(names).expect("names file");
    let (mut ok, mut failed) = (0, 0);
    for name in names.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let existed = pak.file_exists(name);
        match pak.add_file_from_file(Path::new(root).join(name), name, false) {
            Ok(_) => {
                println!("{} {name}", if existed { "REPLACED" } else { "ADDED" });
                ok += 1;
            }
            Err(e) => {
                println!("FAILED {name}: {e}");
                failed += 1;
            }
        }
    }
    pak.close().expect("close pak");
    eprintln!("written {ok}, failed {failed}");
}
