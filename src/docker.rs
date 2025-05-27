use std::io::{self, Error};
use std::process::Command;

pub fn list_running_containers() -> io::Result<Vec<String>> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(
            io::ErrorKind::Other,
            format!("Docker command failed: {}", stderr),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = stdout
        .lines()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .collect();
    Ok(lines)
}

pub fn start_container(name: &str, image: &str, ports: &[u16]) -> io::Result<()> {
    // Validate inputs to prevent command injection
    if name.is_empty() || name.contains(' ') || name.contains(';') {
        return Err(Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid container name",
        ));
    }
    if image.is_empty() || image.contains(' ') || image.contains(';') {
        return Err(Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid image name",
        ));
    }
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        name.to_string(),
    ];

    for port in ports {
        let port_mapping = format!("{}:{}", port, port);
        args.push("-p".to_string());
        args.push(port_mapping);
    }
    args.push(image.to_string());

    let status = Command::new("docker").args(&args).status()?;
    if !status.success() {
        return Err(Error::new(io::ErrorKind::Other, "Docker command failed"));
    }

    Ok(())
}

pub fn stop_container(name: &str) -> io::Result<()> {
    // Validate container name to prevent command injection
    if name.is_empty() || name.contains(' ') || name.contains(';') {
        return Err(Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid container name",
        ));
    }

    let status = Command::new("docker").args(["rm", "-f", name]).status()?;

    if !status.success() {
        return Err(Error::new(io::ErrorKind::Other, "Failed to stop container"));
    }
    Ok(())
}
