//! Filesystem helpers for file transfers, shared by hosts and viewers that
//! write received files to disk. Everything here treats the *sender* as
//! untrusted.

/// Reduce an untrusted filename to one safe path component. Path separators
/// and traversal are stripped; control characters and Windows-reserved
/// characters are replaced; empty results get a placeholder.
pub fn sanitize_filename(name: &str) -> String {
    // Take the final path component whichever separator the sender used.
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches([' ', '.']).to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "unnamed-file".to_string()
    } else {
        trimmed
    }
}

/// Pick a non-clobbering destination path: `name`, `name (1)`, `name (2)`, …
pub fn unique_destination(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 1..10_000 {
        let c = dir.join(format!("{stem} ({i}){ext}"));
        if !c.exists() {
            return c;
        }
    }
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    dir.join(format!("{stem} ({unix}){ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_paths_and_reserved_chars() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(sanitize_filename("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize_filename("con trol\u{7}.txt"), "con trol_.txt");
        assert_eq!(sanitize_filename(""), "unnamed-file");
        assert_eq!(sanitize_filename(".."), "unnamed-file");
        assert_eq!(sanitize_filename("..."), "unnamed-file");
    }

    #[test]
    fn unique_destination_avoids_clobbering() {
        let dir = std::env::temp_dir().join(format!("ndsp-files-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_destination(&dir, "a.txt");
        assert_eq!(first, dir.join("a.txt"));
        std::fs::write(&first, b"x").unwrap();
        let second = unique_destination(&dir, "a.txt");
        assert_eq!(second, dir.join("a (1).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
