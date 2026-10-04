use hider::{Manifest, ManifestSplitter};
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

fn find_files_recursive(folder_path: PathBuf) -> Result<Vec<PathBuf>, std::io::Error> {
    let whitelisted_extensions = vec!["dll", "exe"];
    let mut files: Vec<PathBuf> = Vec::new();

    let stuff = fs::read_dir(&folder_path)?;
    for item in stuff {
        let item = item?.path();
        if item.is_file() {
            if let Some(extension) = item.extension() {
                if whitelisted_extensions.iter().any(|ex| *ex == extension.to_string_lossy()) {
                    files.push(item);
                }
            }
        } else {
            files.append(&mut find_files_recursive(item)?);
        }
    }
    Ok(files)
}

fn print_help() {
    println!("Usage:");
    println!("  hide <mode> <target_folder> <data_file_path>");
    println!();
    println!("Modes:");
    println!("  hide      Hide data within DLL files in target directory");
    println!("  restore   Reconstruct original file from manifest");
    println!("  clean     Remove hidden data using manifest");
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = env::args().collect::<Vec<String>>();

    let mode = match args.get(1) {
        Some(m) if m == "-h" || m == "--help" => {
            print_help();
            return Ok(());
        }
        Some(m) => m.as_str(),
        None => {
            print_help();
            return Err("Specify mode (e.g., hide, restore, clean)".into());
        }
    };

    match mode {
        "hide" => {
            let target_folder = args
                .get(2)
                .ok_or_else(|| Box::<dyn std::error::Error>::from("Specify target folder"))?;

            let data_file_path = args
                .get(3)
                .ok_or_else(|| Box::<dyn std::error::Error>::from("Specify your file path"))?;

            // Check target folder
            let target_folder = Path::new(target_folder).to_path_buf();
            if !target_folder.exists() {
                return Err("Target folder doesn't exist".into());
            }
            if !target_folder.is_dir() {
                return Err("Target folder should be a folder".into());
            }
            let target_folder = target_folder.canonicalize()?;

            // Check file to hide
            let data_file_path = Path::new(data_file_path).to_path_buf();
            if !data_file_path.exists() {
                return Err("Target file doesn't exist".into());
            }
            if !data_file_path.is_file() {
                return Err("Target file should be a file".into());
            }
            let data_file_path = data_file_path.canonicalize()?;

            let data_file = fs::read(&data_file_path)?;
            if data_file.is_empty() {
                return Err("Target file is empty".into());
            }

            let files = find_files_recursive(target_folder)?;
            let files: Vec<&PathBuf> = files.iter().filter(|file| !file.is_dir()).collect();

            if files.is_empty() {
                return Err("No DLL files found in the specified directory".into());
            }

            let total_files = files.len() as f32;
            let k = (total_files * 0.5).ceil() as usize;
            let m = files.len() - k;

            ManifestSplitter::spread_to_targets(
                &data_file,
                &files,
                Path::new("./"),
                k,
                m,
            )?;

            println!("Data successfully spread and manifest generated.");
        }
        "restore" => {
            let manifest_path = args.get(2).map(Path::new).unwrap_or_else(|| Path::new("manifest.json"));
            let output_dir = args.get(3).map(Path::new).unwrap_or_else(|| Path::new("./restored"));

            if !manifest_path.exists() {
                return Err("Manifest file not found".into());
            }

            let manifest = Manifest::load_from_file(manifest_path)?;
            ManifestSplitter::construct_from_manifest(&manifest, output_dir)?;
            println!("File successfully restored to {:?}", output_dir);
        }
        "clean" => {
            let manifest_path = args.get(2).map(Path::new).unwrap_or_else(|| Path::new("manifest.json"));

            if !manifest_path.exists() {
                return Err("Manifest file not found".into());
            }

            let manifest = Manifest::load_from_file(manifest_path)?;
            ManifestSplitter::delete_from_manifest(&manifest);
            println!("Hidden segments successfully removed.");
        }
        _ => {
            print_help();
            return Err(format!("Unknown mode: {}", mode).into());
        }
    }

    Ok(())
}