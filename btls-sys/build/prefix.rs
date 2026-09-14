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

pub fn prefix_symbols(config: &Config) {
    // List static libraries to prefix symbols in
    let static_libs: Vec<PathBuf> = [
        config.out_dir.join("build"),
        config.out_dir.join("build").join("ssl"),
        config.out_dir.join("build").join("crypto"),
    ]
    .iter()
    .flat_map(|dir| {
        ["libssl.a", "libcrypto.a"]
            .into_iter()
            .map(move |file| PathBuf::from(dir).join(file))
    })
    .filter(|p| p.exists())
    .collect();

    let apple = matches!(&*config.target_os, "macos" | "ios");

    // Use `nm` to list symbols in these static libraries
    let nm = match &*config.target_os {
        "android" => android_toolchain(config).join("llvm-nm"),
        _ if apple => apple_llvm_tool("llvm-nm"),
        _ => PathBuf::from("nm"),
    };
    let out = run_command(Command::new(nm).args(&static_libs)).unwrap();
    let types: &[&str] = if apple {
        &[" T ", " D ", " B ", " C ", " R ", " S ", " W "]
    } else {
        &[" T ", " D ", " B ", " C ", " R ", " W "]
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
            } else {
                (!l.starts_with('_')).then(|| format!("{l} {PREFIX}_{l}"))
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
