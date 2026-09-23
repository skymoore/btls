use crate::{config::Config, pick_best_android_ndk_toolchain, run_command};
use std::{fs, io::Write, path::PathBuf, process::Command};

// The prefix to add to all symbols, to avoid collisions with other BoringSSL copies.
// (`CARGO_CRATE_NAME` would be `build_script_main` inside a build script.)
const PREFIX: &str = "btls_sys";

// Callback to add a `link_name` macro with the prefix to all generated bindings.
// bindgen emits the override as an already-mangled name (`\u{1}` prefix), so on
// Mach-O targets it must carry the platform's leading `_` itself.
#[derive(Debug)]
pub struct PrefixCallback {
    pub mach_o: bool,
}

impl bindgen::callbacks::ParseCallbacks for PrefixCallback {
    fn generated_link_name_override(
        &self,
        item_info: bindgen::callbacks::ItemInfo<'_>,
    ) -> Option<String> {
        let lead = if self.mach_o { "_" } else { "" };
        Some(format!("{lead}{PREFIX}_{}", item_info.name))
    }
}

fn android_toolchain(config: &Config) -> PathBuf {
    let mut android_bin_path = config
        .env
        .android_ndk_home
        .clone()
        .expect("Please set ANDROID_NDK_HOME for Android build");
    android_bin_path.extend(["toolchains", "llvm", "prebuilt"]);
    android_bin_path.push(pick_best_android_ndk_toolchain(&android_bin_path).unwrap());
    android_bin_path.push("bin");
    android_bin_path
}

/// Locate an LLVM binutil for Apple targets: `$LLVM_<TOOL>` env, then PATH, then Homebrew.
fn apple_llvm_tool(name: &str) -> PathBuf {
    let env_key = name.replace('-', "_").to_uppercase();
    if let Some(p) = std::env::var_os(&env_key) {
        return PathBuf::from(p);
    }
    if Command::new(name).arg("--version").output().is_ok() {
        return PathBuf::from(name);
    }
    for dir in ["/opt/homebrew/opt/llvm/bin", "/usr/local/opt/llvm/bin"] {
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return p;
        }
    }
    panic!(
        "btls-sys `prefix-symbols` needs `{name}` on macOS/iOS: run `brew install llvm` or set ${env_key} to its path"
    );
}

/// Locate an LLVM binutil on Windows: `$LLVM_<TOOL>` env, then PATH, then the directory of
/// `$LIBCLANG_PATH` (bindgen already needs it), then the LLVM installer's default location.
fn windows_llvm_tool(name: &str) -> PathBuf {
    let env_key = name.replace('-', "_").to_uppercase();
    if let Some(p) = std::env::var_os(&env_key) {
        return PathBuf::from(p);
    }
    let exe = format!("{name}.exe");
    if Command::new(&exe).arg("--version").output().is_ok() {
        return PathBuf::from(exe);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("LIBCLANG_PATH") {
        candidates.push(PathBuf::from(p).join(&exe));
    }
    candidates.push(PathBuf::from(r"C:\Program Files\LLVM\bin").join(&exe));
    for p in candidates {
        if p.exists() {
            return p;
        }
    }
    panic!(
        "btls-sys `prefix-symbols` needs `{name}` on Windows: install LLVM (winget install LLVM.LLVM) or set ${env_key} to its path"
    );
}

pub fn prefix_symbols(config: &Config) {
    let windows = config.target_os == "windows";

    // List static libraries to prefix symbols in. CMake's multi-config generators (MSVC) put
    // them under a configuration subdirectory (`Debug`, `Release`, …) and name them `ssl.lib`.
    let mut dirs: Vec<PathBuf> = vec![
        config.out_dir.join("build"),
        config.out_dir.join("build").join("ssl"),
        config.out_dir.join("build").join("crypto"),
    ];
    if windows {
        for sub in ["Debug", "Release", "RelWithDebInfo", "MinSizeRel"] {
            dirs.push(config.out_dir.join("build").join(sub));
            dirs.push(config.out_dir.join("build").join("ssl").join(sub));
            dirs.push(config.out_dir.join("build").join("crypto").join(sub));
        }
    }
    let names: &[&str] = if windows {
        &["ssl.lib", "crypto.lib", "libssl.a", "libcrypto.a"]
    } else {
        &["libssl.a", "libcrypto.a"]
    };
    let static_libs: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| names.iter().map(move |file| dir.join(file)))
        .filter(|p| p.exists())
        .collect();
    assert!(
        !static_libs.is_empty(),
        "btls-sys `prefix-symbols`: no ssl/crypto static libraries found under {}",
        config.out_dir.join("build").display()
    );

    let apple = matches!(&*config.target_os, "macos" | "ios");

    // Use `nm` to list symbols in these static libraries
    let nm = match &*config.target_os {
        "android" => android_toolchain(config).join("llvm-nm"),
        _ if apple => apple_llvm_tool("llvm-nm"),
        _ if windows => windows_llvm_tool("llvm-nm"),
        _ => PathBuf::from("nm"),
    };
    let out = run_command(Command::new(nm).args(&static_libs)).unwrap();
    // `V` (weak object: vtables, typeinfo) and `u` (GNU unique: C++ inline statics) are ELF-only
    // kinds that C++ BoringSSL emits; without them two BoringSSL copies still collide.
    let types: &[&str] = if apple {
        &[" T ", " D ", " B ", " C ", " R ", " S ", " W "]
    } else {
        &[" T ", " D ", " B ", " C ", " R ", " W ", " V ", " u "]
    };
    let mut redefine_syms: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| types.iter().any(|s| l.contains(s)))
        .filter_map(|l| l.split_whitespace().nth(2).map(|s| s.to_string()))
        .filter_map(|l| {
            if apple {
                // Mach-O C symbols carry a leading `_`; anything else is linker-local.
                l.strip_prefix('_')
                    .map(|c| format!("_{c} _{PREFIX}_{c}"))
            } else if windows {
                // COFF x64: C symbols carry no leading `_`; MSVC-mangled C++ names start with `?`
                // (`?…@bssl@@…`). Names starting with `_` are CRT/compiler symbols (`_fltused`,
                // `__security_cookie`) and stay. The linker treats names as opaque strings, so a
                // prefixed mangled name is fine as long as every object in the archive agrees.
                (!l.starts_with('_')).then(|| format!("{l} {PREFIX}_{l}"))
            } else {
                // ELF C symbols have no leading `_`. Itanium-mangled C++ symbols (`_Z...`, the whole
                // `bssl::` namespace, vtables, typeinfo) do; they must be prefixed too or the ssl/
                // crypto C++ internals collide with another BoringSSL in the same binary — the
                // Apple branch above already renames them via the stripped-underscore path.
                (!l.starts_with('_') || l.starts_with("_Z")).then(|| format!("{l} {PREFIX}_{l}"))
            }
        })
        .collect();
    redefine_syms.sort();
    redefine_syms.dedup();

    let redefine_syms_path = config.out_dir.join("redefine_syms.txt");
    let mut f = fs::File::create(&redefine_syms_path).unwrap();
    for sym in &redefine_syms {
        writeln!(f, "{sym}").unwrap();
    }
    f.flush().unwrap();

    // Use `objcopy` to prefix symbols in these static libraries
    let objcopy = match &*config.target_os {
        "android" => android_toolchain(config).join("llvm-objcopy"),
        _ if apple => apple_llvm_tool("llvm-objcopy"),
        _ if windows => windows_llvm_tool("llvm-objcopy"),
        _ => PathBuf::from("objcopy"),
    };
    for static_lib in &static_libs {
        run_command(
            Command::new(&objcopy)
                .arg(format!("--redefine-syms={}", redefine_syms_path.display()))
                .arg(static_lib),
        )
        .unwrap();
    }
}
