use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Evidence {
    file: File,
    path: PathBuf,
}

impl Evidence {
    pub fn create(repo: &Path) -> io::Result<Self> {
        Self::create_named(repo, "engine-crash", None)
    }

    pub fn create_ublk(repo: &Path, fio_version: &str) -> io::Result<Self> {
        Self::create_named(repo, "ublk-fio", Some(fio_version))
    }

    fn create_named(repo: &Path, name: &str, fio_version: Option<&str>) -> io::Result<Self> {
        let commit = command_text("git", &["rev-parse", "HEAD"], repo)?;
        let kernel = command_text("uname", &["-srvm"], repo)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let directory = repo.join("evidence");
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{name}-{timestamp}-{commit}.log"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut evidence = Self { file, path };
        evidence.line(&format!("commit: {commit}"))?;
        evidence.line(&format!("kernel: {kernel}"))?;
        if let Some(version) = fio_version {
            evidence.line(&format!("fio: {version}"))?;
        }
        evidence.line("backing: test-created temporary regular file")?;
        Ok(evidence)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn line(&mut self, line: &str) -> io::Result<()> {
        writeln!(self.file, "{line}")?;
        self.file.flush()
    }
}

#[cfg(test)]
pub fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn command_text(program: &str, args: &[&str], directory: &Path) -> io::Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(directory)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{program} exited with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_files_are_create_new() {
        let path = std::env::temp_dir().join(format!(
            "block-storage-lab-evidence-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut first = create_new(&path).unwrap();
        first.write_all(b"original\n").unwrap();
        assert_eq!(
            create_new(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "original\n");
        fs::remove_file(path).unwrap();
    }
}
