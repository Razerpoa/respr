use hider::{Manifest, ManifestSplitter};
use std::{
    env, error::Error, fs, path::{Path, PathBuf},
};

fn find_files_recursive(folder_path: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let whitelisted_extensions = vec!["dll", "exe"];
    let mut files: Vec<PathBuf> = Vec::new();

    let stuff = fs::read_dir(folder_path)?;
    for item in stuff {
        let item = item?.path();
        if item.is_file() {
            if let Some(extension) = item.extension() {
                if whitelisted_extensions
                    .iter()
                    .any(|ex| *ex == extension.to_string_lossy())
                {
                    files.push(item);
                }
            }
        } else {
            files.append(&mut find_files_recursive(&item)?);
        }
    }
    Ok(files)
}

/// Pull `--root <dir>` / `--root=<dir>` out of an argument list.
///
/// Returns the requested root (if any) alongside the remaining arguments, so
/// the flag can appear anywhere on the command line.
fn take_root_flag(args: &[String]) -> (Option<PathBuf>, Vec<String>) {
    let mut root = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.iter().peekable();

    while let Some(arg) = it.next() {
        if let Some(value) = arg.strip_prefix("--root=") {
            if value.is_empty() {
                eprintln!("[Warning] --root needs a directory; ignoring");
            } else {
                root = Some(PathBuf::from(value));
            }
        } else if arg == "--root" {
            match it.next() {
                Some(value) => root = Some(PathBuf::from(value)),
                None => eprintln!("[Warning] --root needs a directory; ignoring"),
            }
        } else {
            rest.push(arg.clone());
        }
    }

    (root, rest)
}

fn print_help() {
    println!("Usage:");
    println!("  hide <target_folder> <data_file> [manifest_path]");
    println!("  restore [manifest] [output_dir] [--root <dir>]");
    println!();
    println!("  output_dir is a directory; the restored file keeps its original name.");
    println!("  clean [manifest] [--root <dir>]");
    println!();
    println!("Modes:");
    println!("  hide      Spread a file across DLL/EXE hosts in a directory tree");
    println!("  restore   Reconstruct the original file from a manifest");
    println!("  clean     Remove hidden data using a manifest");
    println!();
    println!("Options:");
    println!("  --root <dir>  Folder holding the hosts. The manifest records where they");
    println!("                were hidden, so this is only needed once that tree moves.");
    println!();
    println!("Defaults:");
    println!("  manifest_path  ./manifest.json");
    println!("  output_dir     ./restored");
}

fn main() -> Result<(), Box<dyn Error>> {
    let raw_args = env::args().collect::<Vec<String>>();

    let (root_override, rest) = take_root_flag(&raw_args[1..]);

    // Re-attach the program name so argument indices match the usage strings.
    let mut args: Vec<String> = Vec::with_capacity(rest.len() + 1);
    args.push(raw_args[0].clone());
    args.extend(rest);

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

            let files = find_files_recursive(&target_folder)?;
            let files: Vec<&PathBuf> = files.iter().filter(|file| !file.is_dir()).collect();

            if files.is_empty() {
                return Err("No DLL files found in the specified directory".into());
            }
            
            // if files.len() < 10 {
            //     let mut user_input = String::new();
            //     print!("Total files are below 10, not recommended for use. Continue? (y/n): ");
            //     loop {
            //         io::stdout().flush()?;
            //         io::stdin().read_line(&mut user_input)?;
            //         user_input = user_input.trim().to_lowercase();
            //         if user_input == "n" {
            //             return Ok(());
            //         } 
            //         if user_input == "y" {
            //             break;
            //         }
            //         print!("Continue? (y/n): ");
            //     }
            // }
            
            let total_files = files.len() as f32;
            let k = (total_files * 0.5).ceil() as usize;
            let m = files.len() - k;

            // The manifest is written wherever the user asks, so it can be kept
            // away from the host tree. Segment paths stay relative to the tree.
            let manifest_output = args
                .get(4)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./"));

            let source_name = data_file_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());

            let manifest = ManifestSplitter::spread_to_targets(
                &data_file,
                &files,
                &target_folder,
                &manifest_output,
                source_name.as_deref(),
                k,
                m,
            )?;

            let manifest_path = if manifest_output.is_dir() {
                manifest_output.join("manifest.json")
            } else {
                manifest_output
            };

            println!(
                "Hid {} bytes as {}+{} segments under {:?}",
                data_file.len(),
                k,
                m,
                manifest.root
            );
            println!("Manifest: {:?}", manifest_path);
            println!("Restore with this manifest alone unless the host tree moves.");
        }
        "restore" => {
            let manifest_path = args
                .get(2)
                .map(Path::new)
                .unwrap_or_else(|| Path::new("manifest.json"));
            let output_dir = args
                .get(3)
                .map(Path::new)
                .unwrap_or_else(|| Path::new("./restored"));
            println!("Writing restored file into: {:?}", output_dir);

            if !manifest_path.exists() {
                return Err("Manifest file not found".into());
            }

            let manifest = Manifest::load_from_file(manifest_path)?;
            let manifest_dir = manifest_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let root = manifest.pick_root(root_override.as_deref(), &manifest_dir);
            println!("Using root: {:?}", root);

            ManifestSplitter::construct_from_manifest(&manifest, &root, output_dir)?;
            println!("File successfully restored to {:?}", output_dir);
        }
        "clean" => {
            let manifest_path = args
                .get(2)
                .map(Path::new)
                .unwrap_or_else(|| Path::new("manifest.json"));

            if !manifest_path.exists() {
                return Err("Manifest file not found".into());
            }

            let manifest = Manifest::load_from_file(manifest_path)?;
            let manifest_dir = manifest_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let root = manifest.pick_root(root_override.as_deref(), &manifest_dir);
            println!("Using root: {:?}", root);

            ManifestSplitter::delete_from_manifest(&manifest, &root);
            println!("Hidden segments successfully removed.");
        }
        _ => {
            print_help();
            return Err(format!("Unknown mode: {}", mode).into());
        }
    }

    Ok(())
}
