use crate::cli::ListArgs;
use crate::crypto::resolve_archive_password;
use crate::util::{load_archive, read_header};
use anyhow::Result;

pub fn run(args: ListArgs) -> Result<()> {
    let header = read_header(&args.archive)?;
    let password = resolve_archive_password(&args.password, header.is_encrypted())?;
    let archive = load_archive(&args.archive, password.as_deref())?;
    let has_dictionary = archive.dictionary.is_some();

    println!("archive\t{}", args.archive.display());
    println!("version\t{}", archive.header.version);
    println!(
        "encrypted\t{}",
        if archive.header.is_encrypted() {
            "yes"
        } else {
            "no"
        }
    );
    println!("size\t{}", archive.archive_size);
    println!("files\t{}", archive.index.files.len());
    println!("chunks\t{}", archive.index.chunks.len());
    println!("chunk_size\t{}", archive.header.chunk_size);
    println!("dictionary\t{}", if has_dictionary { "yes" } else { "no" });
    println!(
        "dictionary_size\t{}",
        archive.dictionary.as_ref().map_or(0, |bytes| bytes.len())
    );
    println!("index_size\t{}", archive.footer.index_size);

    for entry in &archive.index.files {
        println!(
            "{}\t{}\t{}",
            entry.original_size, entry.chunk_id, entry.path
        );
    }

    Ok(())
}
