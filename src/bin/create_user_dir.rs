use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Root path for simulated user home directory (~/)
    let user_home = std::env::current_dir()?.join("simulated_user");

    // 2. Define directory layout
    let directories = [
        "Desktop",
        "Documents/Projects/rust_app",
        "Downloads",
        "Pictures/Screenshots",
        ".config/my_app",
    ];

    println!("Simulating user directory structure at: {:?}\n", user_home);

    for dir in &directories {
        let path = user_home.join(dir);
        fs::create_dir_all(&path)?;
        println!("[DIR]  {}", path.display());
    }

    // 3. Populate sample files
    let files = [
        (
            "Documents/Projects/rust_app/main.rs",
            "fn main() { println!(\"Hello, world!\"); }",
        ),
        (
            ".config/my_app/config.json",
            "{\n  \"theme\": \"dark\",\n  \"notifications\": true\n}",
        ),
        (
            "Desktop/notes.txt",
            "1. Buy groceries\n2. Review PRs\n3. Clean temporary files",
        ),
        ("Downloads/sample.txt", "Downloaded file contents go here."),
    ];

    println!();
    for (rel_path, content) in &files {
        let file_path = user_home.join(rel_path);
        fs::write(&file_path, content)?;
        println!("[FILE] {}", file_path.display());
    }

    println!("\nDirectory tree initialized successfully.");

    Ok(())
}
