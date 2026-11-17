//! Rezn's bounded Docker image reference grammar, shared by IR v1 and IR v2.
use anyhow::{bail, ensure, Context, Result};

// Deliberately bounded Docker reference grammar; availability is a backend concern.
pub fn validate_image(image: &str) -> Result<()> {
    ensure!(
        !image.is_empty() && image.len() <= 255,
        "image must be a nonempty Docker reference up to 255 bytes"
    );
    let (reference, digest) = match image.split_once('@') {
        Some((r, d)) => (r, Some(d)),
        None => (image, None),
    };
    if let Some(digest) = digest {
        let hash = digest
            .strip_prefix("sha256:")
            .context("image digest must be sha256")?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "image sha256 digest must have 64 hex digits"
        );
    }
    let slash = reference.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (path, tag) = match reference[slash..].split_once(':') {
        Some((_, tag)) => (&reference[..reference.len() - tag.len() - 1], Some(tag)),
        None => (reference, None),
    };
    if let Some(tag) = tag {
        ensure!(
            !tag.is_empty()
                && tag.len() <= 128
                && (tag.as_bytes()[0].is_ascii_alphanumeric() || tag.starts_with('_'))
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid image tag"
        );
    }
    let components: Vec<_> = path.split('/').collect();
    for (i, component) in components.iter().enumerate() {
        if i == 0
            && components.len() > 1
            && (component.contains('.') || component.contains(':') || *component == "localhost")
        {
            let (host, port) = component
                .split_once(':')
                .map(|(h, p)| (h, Some(p)))
                .unwrap_or((component, None));
            ensure!(
                !host.is_empty()
                    && host.split('.').all(|part| !part.is_empty()
                        && part.as_bytes()[0].is_ascii_alphanumeric()
                        && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
                "invalid image registry"
            );
            if let Some(port) = port {
                ensure!(
                    port.parse::<u16>().is_ok_and(|p| p > 0),
                    "invalid image registry port"
                );
            }
            continue;
        }
        let bytes = component.as_bytes();
        ensure!(
            !bytes.is_empty()
                && bytes[0].is_ascii_lowercase_or_digit()
                && bytes[bytes.len() - 1].is_ascii_lowercase_or_digit(),
            "invalid image repository component"
        );
        let mut pos = 0;
        while pos < bytes.len() {
            if bytes[pos].is_ascii_lowercase_or_digit() {
                pos += 1;
                continue;
            }
            match bytes[pos] {
                b'.' => pos += 1,
                b'_' => {
                    pos += 1;
                    if bytes.get(pos) == Some(&b'_') {
                        pos += 1;
                    }
                }
                b'-' => {
                    while bytes.get(pos) == Some(&b'-') {
                        pos += 1;
                    }
                }
                _ => bail!("invalid image repository character"),
            }
            ensure!(
                bytes
                    .get(pos)
                    .is_some_and(|b| b.is_ascii_lowercase_or_digit()),
                "invalid image repository separator"
            );
        }
    }
    Ok(())
}

trait LowercaseOrDigit {
    fn is_ascii_lowercase_or_digit(&self) -> bool;
}
impl LowercaseOrDigit for u8 {
    fn is_ascii_lowercase_or_digit(&self) -> bool {
        self.is_ascii_lowercase() || self.is_ascii_digit()
    }
}
