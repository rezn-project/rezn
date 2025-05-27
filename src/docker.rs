use std::io::{self, Error};
use std::process::Command;

pub fn list_running_containers() -> io::Result<Vec<String>> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = stdout.lines().map(|s| s.to_string()).collect();
    Ok(lines)
}

pub fn start_container(name: &str, image: &str, ports: &[u16]) -> io::Result<()> {
    let mut args = vec!["run", "-d", "--name", name];
    for port in ports {
        args.push("-p");
        args.push(Box::leak(format!("{}:{}", port, port).into_boxed_str()));
    }
    args.push(image);

    Command::new("docker").args(&args).spawn()?.wait()?;
    Ok(())
}

pub fn stop_container(name: &str) -> io::Result<()> {
    Command::new("docker")
        .args(["rm", "-f", name])
        .spawn()?
        .wait()?;
    Ok(())
}
