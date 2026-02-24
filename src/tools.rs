use std::path::Path;

use tracing::debug;

pub async fn execute_bash(command: &str) -> String {
    debug!(command, "executing bash command");

    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let mut result = String::new();

            if !stdout.is_empty() {
                result.push_str(&stdout);
            }

            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("STDERR:\n");
                result.push_str(&stderr);
            }

            if result.is_empty() {
                format!("Command completed with exit code {exit_code}")
            } else if exit_code != 0 {
                format!("{result}\nExit code: {exit_code}")
            } else {
                result.to_string()
            }
        }
        Err(e) => format!("Failed to execute command: {e}"),
    }
}

pub async fn read_file(path: &str) -> String {
    debug!(path, "reading file");

    match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(e) => format!("Error reading file: {e}"),
    }
}

pub async fn write_file(path: &str, content: &str) -> String {
    debug!(path, "writing file");

    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return format!("Error creating directories: {e}");
            }
        }
    }

    match tokio::fs::write(path, content).await {
        Ok(()) => format!("Successfully wrote to {path}"),
        Err(e) => format!("Error writing file: {e}"),
    }
}

pub async fn list_files(path: &str) -> String {
    debug!(path, "listing files");

    match tokio::fs::read_dir(path).await {
        Ok(mut entries) => {
            let mut files = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                let suffix = match entry.file_type().await {
                    Ok(ft) if ft.is_dir() => "/",
                    Ok(ft) if ft.is_symlink() => "@",
                    _ => "",
                };
                files.push(format!("{name}{suffix}"));
            }
            files.sort();
            if files.is_empty() {
                "Directory is empty".to_string()
            } else {
                files.join("\n")
            }
        }
        Err(e) => format!("Error listing directory: {e}"),
    }
}

pub fn definitions() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "bash",
            "description": "Execute a bash command and return its output (stdout and stderr).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    }
                },
                "required": ["command"]
            }
        }),
        serde_json::json!({
            "name": "read_file",
            "description": "Read the contents of a file at the given path.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the file to read"
                    }
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "write_file",
            "description": "Write content to a file, creating parent directories as needed.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }
        }),
        serde_json::json!({
            "name": "list_files",
            "description": "List files and directories at the given path.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path to list"
                    }
                },
                "required": ["path"]
            }
        }),
    ]
}
