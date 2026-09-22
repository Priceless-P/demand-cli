use minisign_verify::{PublicKey, Signature};
use self_update::{
    backends, cargo_crate_version,
    update::{ReleaseAsset, UpdateStatus},
    TempDir,
};
use tracing::{debug, error, info, warn};

const REPO_OWNER: &str = "dmnd-pool";
const REPO_NAME: &str = "dmnd-client";
const BIN_NAME: &str = "dmnd-client";
const RELEASE_PUBLIC_KEY: &str = include_str!("../release-signing.pub");

pub fn check_update_proxy() {
    info!("Checking for latest released version...");
    // Determine the OS and map to the asset name
    let os = std::env::consts::OS;
    let target_bin = match os {
        "linux" => "dmnd-client-linux",
        "macos" => "dmnd-client-macos",
        "windows" => "dmnd-client-windows.exe",
        _ => {
            error!("Warning: Unsupported OS '{}', skipping update", os);
            unreachable!()
        }
    };

    debug!("OS: {}", target_bin);
    debug!("DMND Client version: {}", cargo_crate_version!());
    let original_path = std::env::current_exe().expect("Failed to get current executable path");
    let tmp_dir = TempDir::new_in(::std::env::current_dir().expect("Failed to get current dir"))
        .expect("Failed to create tmp dir");

    let updater = match backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(cargo_crate_version!())
        .target(target_bin)
        .show_output(false)
        .no_confirm(true)
        .bin_install_path(tmp_dir.path())
        .build()
    {
        Ok(updater) => updater,
        Err(e) => {
            error!("Failed to configure update: {}", e);
            return;
        }
    };

    let latest_release = match updater.get_latest_release() {
        Ok(release) => release,
        Err(e) => {
            error!("Failed to check the latest release: {}", e);
            return;
        }
    };
    if !self_update::version::bump_is_greater(
        cargo_crate_version!(),
        &latest_release.version,
    )
    .unwrap_or(false)
    {
        info!("Package is up to date");
        return;
    }

    let signature_name = format!("{target_bin}.minisig");
    if !latest_release
        .assets
        .iter()
        .any(|asset| asset.name == target_bin)
        || !latest_release
            .assets
            .iter()
            .any(|asset| asset.name == signature_name)
    {
        warn!(
            "Latest release v{} has no signed update for {}; skipping update",
            latest_release.version, target_bin
        );
        return;
    }

    match updater.update_extended() {
        Ok(status) => match status {
            UpdateStatus::UpToDate => {
                info!("Package is up to date");
            }
            UpdateStatus::Updated(release) => {
                info!(
                    "Proxy updated to version {}. Restarting Proxy",
                    release.version
                );
                for asset in &release.assets {
                    if asset.name == target_bin {
                        let bin_name = std::path::PathBuf::from(target_bin);
                        let new_exe = tmp_dir.path().join(&bin_name);
                        let mut file =
                            std::fs::File::create(&new_exe).expect("Failed to create file");
                        let mut download = self_update::Download::from_url(&asset.download_url);
                        download.set_header(
                            reqwest::header::ACCEPT,
                            reqwest::header::HeaderValue::from_static("application/octet-stream"), // to triggers a redirect to the actual binary.
                        );
                        download
                            .download_to(&mut file)
                            .expect("Failed to download file");
                    }
                }
                let bin_name = std::path::PathBuf::from(target_bin);
                let new_exe = tmp_dir.path().join(&bin_name);
                if let Err(e) =
                    verify_signature(&release.assets, target_bin, &new_exe, tmp_dir.path())
                {
                    error!("Signature verification failed: {}, Skipping update", e);
                    return;
                }
                if let Err(e) = std::fs::rename(&new_exe, &original_path) {
                    error!(
                        "Failed to move new binary to {}: {}",
                        original_path.display(),
                        e
                    );
                    return;
                }

                let _ = std::fs::remove_dir_all(tmp_dir); // clean up tmp dir
                                                          // Get original cli rgs
                let args = std::env::args().skip(1).collect::<Vec<_>>();

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    use std::os::unix::process::CommandExt;
                    // On Unix-like systems, replace the current process with the new binary
                    if let Err(e) = std::fs::set_permissions(
                        &original_path,
                        std::fs::Permissions::from_mode(0o755),
                    ) {
                        error!(
                            "Failed to set executable permissions on {}: {}",
                            original_path.display(),
                            e
                        );
                        return;
                    }

                    let err = std::process::Command::new(&original_path)
                        .args(&args)
                        .exec();
                    // If exec fails, log the error and exit
                    error!("Failed to exec new binary: {:?}", err);
                    std::process::exit(1);
                }
                #[cfg(not(unix))]
                {
                    // On Windows, spawn the new process and exit the current one
                    std::process::Command::new(&original_path)
                        .args(&args)
                        .spawn()
                        .expect("Failed to start proxy");
                    std::process::exit(0);
                }
            }
        },
        Err(e) => {
            error!("Failed to update proxy: {}", e);
        }
    }
}

fn verify_signature(
    assets: &[ReleaseAsset],
    target_bin: &str,
    binary: &std::path::Path,
    temp_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let signature_name = format!("{target_bin}.minisig");
    let signature_asset = assets
        .iter()
        .find(|asset| asset.name == signature_name)
        .ok_or_else(|| format!("Release is missing {signature_name}"))?;
    let signature_path = temp_dir.join(signature_name);
    let mut signature_file = std::fs::File::create(&signature_path)?;
    self_update::Download::from_url(&signature_asset.download_url)
        .download_to(&mut signature_file)?;

    let public_key = PublicKey::decode(RELEASE_PUBLIC_KEY)?;
    let signature = Signature::from_file(&signature_path)?;
    public_key.verify(&std::fs::read(binary)?, &signature, false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_public_key_is_valid() {
        assert!(PublicKey::decode(RELEASE_PUBLIC_KEY).is_ok());
    }
}
