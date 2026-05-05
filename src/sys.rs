//! Runtime-loaded AudioToolbox + supporting framework handles.
//!
//! Loaded once via `OnceLock` on first use and cached for the process
//! lifetime. If any framework fails to dlopen the cache stores the
//! error so subsequent calls don't repeatedly hammer dyld.

use libloading::Library;
use std::sync::OnceLock;

/// Handles to the frameworks the AT bridge needs.
pub struct Framework {
    pub audio_toolbox: Library,
    pub core_foundation: Library,
}

/// Process-wide cache. `OnceLock` so concurrent first calls collapse
/// to a single load.
static FRAMEWORK: OnceLock<Result<Framework, String>> = OnceLock::new();

/// Get (or load) the framework handles. Returns the cached `Err` if a
/// previous load attempt failed.
pub fn framework() -> Result<&'static Framework, &'static str> {
    FRAMEWORK
        .get_or_init(load)
        .as_ref()
        .map_err(|s| s.as_str())
}

fn load() -> Result<Framework, String> {
    let audio_toolbox = open("/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox")?;
    let core_foundation =
        open("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;
    Ok(Framework {
        audio_toolbox,
        core_foundation,
    })
}

fn open(path: &str) -> Result<Library, String> {
    // SAFETY: dlopen on a fixed system framework path with no init
    // callbacks; equivalent to a normal program startup load.
    unsafe { Library::new(path) }.map_err(|e| format!("dlopen {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: every framework on this Mac loads cleanly + a
    /// stable AT entry point resolves.
    #[test]
    fn frameworks_load() {
        let fw = framework().expect("framework load");
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.audio_toolbox
                .get(b"AudioConverterNew\0")
                .expect("AudioConverterNew symbol")
        };
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.core_foundation
                .get(b"CFRetain\0")
                .expect("CFRetain symbol")
        };
    }
}
