use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use eyre::{Context, OptionExt, Result};
use tracing::{debug, info};

use crate::game::mod_loader::{ModLoader, ModLoaderKind};

/// Creates the command for launching a game directly, without going through a launcher.
///
/// Returns `None` if the game directory has no app bundle, in which case the caller
/// falls back to the generic executable search.
///
/// - Vanilla: the executable inside the app bundle, which runs with the host's architecture.
/// - BepInEx: the Doorstop start script that ships with the BepInEx pack, given the app
///   bundle as its first argument (otherwise it falls back to the Linux executable name).
///   The script is run through Rosetta unless the Doorstop library it will inject has an arm64
///   slice, see [`doorstop_supports_arm64`].
pub fn direct_command(
    mod_loader: &ModLoader,
    vanilla: bool,
    game_dir: &Path,
) -> Result<Option<Command>> {
    let Some(app) = find_app_bundle(game_dir)? else {
        debug!("no app bundle found in game directory");
        return Ok(None);
    };

    if vanilla {
        return app_executable(&app).map(|path| Some(Command::new(path)));
    }

    if !matches!(mod_loader.kind, ModLoaderKind::BepInEx { .. }) {
        return Ok(None);
    }

    let script = super::find_executable(game_dir)
        .context("failed to find the BepInEx start script in the game directory")?;
    let app_name = app.file_name().ok_or_eyre("app bundle has no file name")?;

    let mut command = if doorstop_supports_arm64(game_dir) {
        info!("doorstop has an arm64 slice, launching natively");
        Command::new("/bin/bash")
    } else {
        info!("doorstop is x86_64-only, launching through rosetta");

        let mut command = Command::new("/usr/bin/arch");
        command.args(["-x86_64", "/bin/bash"]);
        command
    };

    command.arg(script).arg(app_name);

    Ok(Some(command))
}

/// Whether the game can be started as an arm64 process with Doorstop injected.
///
/// Doorstop can only be injected into a process of its own architecture, and the libraries in
/// current BepInEx 5 packs (`doorstop_libs/libdoorstop_x64.dylib`) are x86_64-only, which forces the
/// game to run through Rosetta. Newer Doorstop builds ship a universal `libdoorstop.dylib`, either
/// in `doorstop_libs` or next to the start script. If the pack has one, we leave the choice of
/// architecture to its script.
fn doorstop_supports_arm64(game_dir: &Path) -> bool {
    [game_dir.to_path_buf(), game_dir.join("doorstop_libs")]
        .iter()
        .filter_map(|dir| dir.read_dir().ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "dylib")
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("libdoorstop"))
        })
        .any(|path| has_arm64_slice(&path).unwrap_or(false))
}

/// Reads just enough of a Mach-O file to tell if it contains an arm64 slice,
/// whether it is thin or a universal ("fat") binary.
fn has_arm64_slice(path: &Path) -> Result<bool> {
    const CPU_TYPE_ARM64: u32 = 0x0100_000c;
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const FAT_MAGIC: u32 = 0xcafe_babe;
    const FAT_MAGIC_64: u32 = 0xcafe_babf;

    let mut file = File::open(path)?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)?;

    let word = |bytes: &[u8]| u32::from_be_bytes(bytes.try_into().unwrap());

    // fat headers are always big-endian, thin ones use the host's byte order (little-endian here)
    match word(&header[..4]) {
        magic @ (FAT_MAGIC | FAT_MAGIC_64) => {
            // struct fat_arch is 20 bytes and fat_arch_64 is 32, both start with the cpu type
            let entry_size = if magic == FAT_MAGIC { 20 } else { 32 };
            let count = word(&header[4..]);

            // cap the count so a corrupt file can't make us read without bound
            for _ in 0..count.min(16) {
                let mut entry = vec![0u8; entry_size];
                file.read_exact(&mut entry)?;

                if word(&entry[..4]) == CPU_TYPE_ARM64 {
                    return Ok(true);
                }
            }

            Ok(false)
        }
        _ if u32::from_le_bytes(header[..4].try_into().unwrap()) == MH_MAGIC_64 => {
            Ok(u32::from_le_bytes(header[4..].try_into().unwrap()) == CPU_TYPE_ARM64)
        }
        _ => Ok(false),
    }
}

/// Finds the first `.app` bundle in the game directory.
fn find_app_bundle(game_dir: &Path) -> Result<Option<PathBuf>> {
    let mut apps = game_dir
        .read_dir()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.extension().is_some_and(|ext| ext == "app"))
        .collect::<Vec<_>>();

    apps.sort();

    Ok(apps.into_iter().next())
}

/// Finds the main executable of an app bundle. Bundles name it after the app
/// by convention, otherwise we accept a single file in `Contents/MacOS`.
fn app_executable(app: &Path) -> Result<PathBuf> {
    let macos_dir = app.join("Contents/MacOS");

    if let Some(stem) = app.file_stem() {
        let conventional = macos_dir.join(stem);
        if conventional.is_file() {
            return Ok(conventional);
        }
    }

    let mut files = fs::read_dir(&macos_dir)
        .with_context(|| format!("failed to read {}", macos_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file());

    match (files.next(), files.next()) {
        (Some(path), None) => Ok(path),
        _ => Err(eyre::eyre!(
            "could not determine the executable of {}",
            app.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn bepinex() -> ModLoader<'static> {
        ModLoader {
            package_name: None,
            file_target: None,
            kind: ModLoaderKind::BepInEx {
                extra_subdirs: Vec::new(),
            },
        }
    }

    fn game_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        let macos_dir = dir.path().join("Valheim.app/Contents/MacOS");
        fs::create_dir_all(&macos_dir).unwrap();
        fs::write(macos_dir.join("Valheim"), "").unwrap();

        for name in ["start_server_bepinex.sh", "start_game_bepinex.sh"] {
            fs::write(dir.path().join(name), "").unwrap();
        }

        dir
    }

    #[test]
    fn vanilla_runs_app_executable() {
        let dir = game_dir();

        let command = direct_command(&bepinex(), true, dir.path())
            .unwrap()
            .unwrap();

        assert_eq!(
            Path::new(command.get_program()),
            dir.path().join("Valheim.app/Contents/MacOS/Valheim")
        );
        assert_eq!(command.get_args().count(), 0);
    }

    #[test]
    fn bepinex_runs_script_under_rosetta() {
        let dir = game_dir();

        let command = direct_command(&bepinex(), false, dir.path())
            .unwrap()
            .unwrap();
        let args = command.get_args().collect::<Vec<_>>();

        assert_eq!(command.get_program(), "/usr/bin/arch");
        assert_eq!(
            args,
            [
                OsStr::new("-x86_64"),
                OsStr::new("/bin/bash"),
                dir.path().join("start_game_bepinex.sh").as_os_str(),
                OsStr::new("Valheim.app"),
            ]
        );
    }

    #[test]
    fn other_loaders_use_generic_search() {
        let dir = game_dir();
        let mut mod_loader = bepinex();
        mod_loader.kind = ModLoaderKind::Lovely {};

        assert!(
            direct_command(&mod_loader, false, dir.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn no_app_bundle_uses_generic_search() {
        let dir = tempfile::tempdir().unwrap();

        assert!(
            direct_command(&bepinex(), false, dir.path())
                .unwrap()
                .is_none()
        );
    }

    const CPU_TYPE_X86_64: u32 = 0x0100_0007;
    const CPU_TYPE_ARM64: u32 = 0x0100_000c;

    fn thin(cpu_type: u32) -> Vec<u8> {
        let mut bytes = 0xfeed_facf_u32.to_le_bytes().to_vec();
        bytes.extend(cpu_type.to_le_bytes());
        bytes
    }

    fn fat(magic: u32, entry_size: usize, cpu_types: &[u32]) -> Vec<u8> {
        let mut bytes = magic.to_be_bytes().to_vec();
        bytes.extend((cpu_types.len() as u32).to_be_bytes());

        for cpu_type in cpu_types {
            let mut entry = vec![0u8; entry_size];
            entry[..4].copy_from_slice(&cpu_type.to_be_bytes());
            bytes.extend(entry);
        }

        bytes
    }

    fn has_arm64(bytes: &[u8]) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.dylib");
        fs::write(&path, bytes).unwrap();

        has_arm64_slice(&path).unwrap_or(false)
    }

    #[test]
    fn detects_arm64_slices() {
        assert!(has_arm64(&thin(CPU_TYPE_ARM64)));
        assert!(!has_arm64(&thin(CPU_TYPE_X86_64)));

        assert!(has_arm64(&fat(
            0xcafe_babe,
            20,
            &[CPU_TYPE_X86_64, CPU_TYPE_ARM64]
        )));
        assert!(has_arm64(&fat(
            0xcafe_babf,
            32,
            &[CPU_TYPE_X86_64, CPU_TYPE_ARM64]
        )));
        assert!(!has_arm64(&fat(0xcafe_babe, 20, &[CPU_TYPE_X86_64])));

        // not Mach-O, truncated, or a corrupt fat header
        assert!(!has_arm64(b"#!/bin/sh\n"));
        assert!(!has_arm64(&[0xca, 0xfe]));
        assert!(!has_arm64(&fat(0xcafe_babe, 20, &[CPU_TYPE_ARM64])[..12]));
    }

    #[test]
    fn x86_64_only_doorstop_uses_rosetta() {
        let dir = game_dir();
        let libs = dir.path().join("doorstop_libs");
        fs::create_dir(&libs).unwrap();
        fs::write(libs.join("libdoorstop_x64.dylib"), thin(CPU_TYPE_X86_64)).unwrap();

        assert!(!doorstop_supports_arm64(dir.path()));
    }

    #[test]
    fn universal_doorstop_launches_natively() {
        let universal = fat(0xcafe_babe, 20, &[CPU_TYPE_X86_64, CPU_TYPE_ARM64]);

        // in doorstop_libs, as in BepInEx 5 packs
        let dir = game_dir();
        let libs = dir.path().join("doorstop_libs");
        fs::create_dir(&libs).unwrap();
        fs::write(libs.join("libdoorstop_x64.dylib"), &universal).unwrap();

        let command = direct_command(&bepinex(), false, dir.path())
            .unwrap()
            .unwrap();
        let args = command.get_args().collect::<Vec<_>>();

        assert_eq!(command.get_program(), "/bin/bash");
        assert_eq!(
            args,
            [
                dir.path().join("start_game_bepinex.sh").as_os_str(),
                OsStr::new("Valheim.app"),
            ]
        );

        // next to the script, as in BepInEx 6
        let dir = game_dir();
        fs::write(dir.path().join("libdoorstop.dylib"), &universal).unwrap();

        assert!(doorstop_supports_arm64(dir.path()));
    }
}
