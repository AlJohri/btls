use crate::{config::Config, pick_best_android_ndk_toolchain, run_command};
use std::{env, fs, io::Write, path::PathBuf, process::Command};

// The prefix to add to all symbols
// Using crate name to avoid collisions with other projects
const PREFIX: &str = env!("CARGO_CRATE_NAME");

// Callback to add a `link_name` macro with the prefix to all generated bindings
#[derive(Debug)]
pub struct PrefixCallback {
    // bindgen emits overrides as `\u{1}name`, which LLVM does not decorate, so
    // Mach-O's leading `_` has to be part of the name.
    pub apple: bool,
}

impl bindgen::callbacks::ParseCallbacks for PrefixCallback {
    fn generated_link_name_override(
        &self,
        item_info: bindgen::callbacks::ItemInfo<'_>,
    ) -> Option<String> {
        let underscore = if self.apple { "_" } else { "" };
        Some(format!("{underscore}{PREFIX}_{}", item_info.name))
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

pub fn is_apple(config: &Config) -> bool {
    matches!(&*config.target_os, "macos" | "ios")
}

// Xcode ships `nm` but no `objcopy`. `BTLS_SYS_LLVM_OBJCOPY` wins, then the
// active Xcode toolchain, then Rust's `llvm-tools` component.
fn apple_llvm_objcopy() -> PathBuf {
    if let Some(path) = env::var_os("BTLS_SYS_LLVM_OBJCOPY") {
        return path.into();
    }
    if let Ok(out) = Command::new("xcrun")
        .args(["--find", "llvm-objcopy"])
        .output()
    {
        if out.status.success() {
            return PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        }
    }
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    if let (Ok(out), Ok(host)) = (
        Command::new(rustc).args(["--print", "sysroot"]).output(),
        env::var("HOST"),
    ) {
        let candidate = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
            .join("lib/rustlib")
            .join(host)
            .join("bin/llvm-objcopy");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("llvm-objcopy")
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

    // Use `nm` to list symbols in these static libraries
    let nm = match &*config.target_os {
        "android" => android_toolchain(config).join("llvm-nm"),
        _ => PathBuf::from("nm"),
    };
    let out = run_command(Command::new(nm).args(&static_libs)).unwrap();
    let apple = is_apple(config);
    // Mach-O `nm` reports data outside __data/__bss as `S`.
    let kinds: &[&str] = if apple {
        &[" T ", " D ", " B ", " C ", " R ", " W ", " S "]
    } else {
        &[" T ", " D ", " B ", " C ", " R ", " W "]
    };
    let mut redefine_syms: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| kinds.iter().any(|s| l.contains(s)))
        .filter_map(|l| l.split_whitespace().nth(2).map(|s| s.to_string()))
        .filter_map(|l| {
            if apple {
                // Mach-O prepends `_` to every C symbol; `__Z...` is C++.
                l.strip_prefix('_')
                    .filter(|name| !name.starts_with('_'))
                    .map(|name| format!("_{name} _{PREFIX}_{name}"))
            } else if l.starts_with('_') {
                None
            } else {
                Some(format!("{l} {PREFIX}_{l}"))
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
        "macos" | "ios" => apple_llvm_objcopy(),
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
