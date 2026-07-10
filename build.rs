fn main() {
    #[cfg(target_os = "windows")]
    {
        // Embed the etr logo as the .exe icon resource (Explorer/taskbar).
        // Cosmetic only — don't fail the build if no resource compiler is available.
        embed_resource::compile("windows/etr.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    {
        // Link against libutempter for utmp/wtmp registration.
        // Prefer the unversioned .so (present when libutempter-devel is installed);
        // fall back to the versioned runtime library by passing its full path.
        let mut candidates = vec![
            "/usr/lib64/libutempter.so".to_string(),
            "/usr/lib64/libutempter.so.0".to_string(),
            "/usr/lib/libutempter.so".to_string(),
            "/usr/lib/libutempter.so.0".to_string(),
        ];
        // Debian/Ubuntu multiarch paths are keyed by the arch triplet
        // (x86_64-linux-gnu, aarch64-linux-gnu, ...) — scan /usr/lib/*/ rather
        // than hardcoding one triplet so this works on any Linux architecture.
        if let Ok(entries) = std::fs::read_dir("/usr/lib") {
            for entry in entries.flatten() {
                let dir = entry.path();
                if dir.is_dir() {
                    candidates.push(dir.join("libutempter.so").display().to_string());
                    candidates.push(dir.join("libutempter.so.0").display().to_string());
                }
            }
        }
        for path in &candidates {
            if std::path::Path::new(path).exists() {
                println!("cargo:rustc-link-arg={path}");
                return;
            }
        }
        eprintln!(
            "cargo:warning=libutempter not found; utmp/wtmp registration will be disabled at runtime"
        );
    }
}
