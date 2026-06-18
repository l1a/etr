fn main() {
    #[cfg(target_os = "linux")]
    {
        // Link against libutempter for utmp/wtmp registration.
        // Prefer the unversioned .so (present when libutempter-devel is installed);
        // fall back to the versioned runtime library by passing its full path.
        let candidates = [
            "/usr/lib64/libutempter.so",
            "/usr/lib/libutempter.so",
            "/usr/lib/x86_64-linux-gnu/libutempter.so",
            "/usr/lib64/libutempter.so.0",
            "/usr/lib/libutempter.so.0",
            "/usr/lib/x86_64-linux-gnu/libutempter.so.0",
        ];
        for path in candidates {
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
