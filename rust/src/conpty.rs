//! Newer Windows pseudo-console backend, embedded so `cargo install` still
//! gives one exe.
//!
//! The console host built into Windows 10 parses everything written to a
//! pseudo-console's input and silently discards sequences it does not know
//! (OSC, DCS, ...). Fixed upstream in Aug 2024 (microsoft/terminal #17741),
//! but the inbox host only gets servicing updates. Without OSC input, Claude
//! never receives the reply to its OSC 7501 query and never reports status.
//!
//! `portable-pty` prefers a sideloaded `conpty.dll` (which launches the
//! `OpenConsole.exe` in its own directory), found by name through
//! `LoadLibrary`. Loading ours by full path first makes that lookup return the
//! already-loaded module. If anything here fails, the inbox backend is used.

#[cfg(all(windows, target_arch = "x86_64"))]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Once;

    const CONPTY_DLL: &[u8] = include_bytes!("../vendor/conpty/conpty.dll");
    const OPEN_CONSOLE: &[u8] = include_bytes!("../vendor/conpty/OpenConsole.exe");
    /// Directory name; bump with the vendored files.
    const VERSION: &str = "1.22.250314001";

    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut core::ffi::c_void;
    }

    pub fn init() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            if std::env::var_os("CPIT_NO_SIDELOAD").is_some() {
                return;
            }
            let _ = load();
        });
    }

    fn load() -> Option<()> {
        let dir = crate::paths::root().join("conpty").join(VERSION);
        std::fs::create_dir_all(&dir).ok()?;
        write_if_changed(&dir.join("OpenConsole.exe"), OPEN_CONSOLE)?;
        let dll = dir.join("conpty.dll");
        write_if_changed(&dll, CONPTY_DLL)?;
        let wide: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        (!handle.is_null()).then_some(())
    }

    /// Skips a file of the right size: a running OpenConsole.exe keeps its
    /// file locked, and the contents only change with `VERSION`.
    fn write_if_changed(path: &Path, bytes: &[u8]) -> Option<()> {
        if std::fs::metadata(path).map(|m| m.len() == bytes.len() as u64).unwrap_or(false) {
            return Some(());
        }
        std::fs::write(path, bytes).ok()
    }
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
mod imp {
    pub fn init() {}
}

pub use imp::init;
