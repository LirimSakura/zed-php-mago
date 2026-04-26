use std::fs;
use zed_extension_api::{self as zed, Result};

const GITHUB_REPO: &str = "LirimSakura/zed-php-mago";

#[inline]
fn bin_name() -> &'static str {
    if zed::current_platform().0 == zed::Os::Windows {
        "mago-lsp-server.exe"
    } else {
        "mago-lsp-server"
    }
}

struct MagoLspExtension {
    cached_binary_path: Option<String>,
}

enum Status {
    None,
    Downloading,
    Failed(String),
}

fn update_status(language_server_id: &zed::LanguageServerId, status: Status) {
    match status {
        Status::None => zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::None,
        ),
        Status::Downloading => zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        ),
        Status::Failed(msg) => zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Failed(msg),
        ),
    }
}

impl MagoLspExtension {
    fn language_server_binary_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        let bin_name = bin_name();
        if let Some(path) = worktree.which(bin_name) {
            return Ok(path);
        }

        if let Some(cached_path) = &self.cached_binary_path {
            if fs::metadata(cached_path).map_or(false, |stat| stat.is_file()) {
                update_status(language_server_id, Status::None);
                return Ok(cached_path.clone());
            }
        }

        if let Some(binary_path) = Self::check_installed() {
            let _ = Self::check_to_update(language_server_id);
            return Ok(binary_path);
        }

        let version_binary_path = Self::check_to_update(language_server_id)?;
        self.cached_binary_path = Some(version_binary_path.clone());
        Ok(version_binary_path)
    }

    fn check_installed() -> Option<String> {
        let entries = fs::read_dir(".").ok()?;
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            let binary_path = entry.path().join(bin_name());
            if fs::metadata(&binary_path).map_or(false, |stat| stat.is_file()) {
                return binary_path.to_str().map(|s| s.to_string());
            }
        }
        None
    }

    fn check_to_update(id: &zed::LanguageServerId) -> Result<String> {
        let (platform, arch) = zed::current_platform();
        let release = zed::latest_github_release(
            GITHUB_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let asset_name = format!(
            "mago-lsp-server-{os}-{arch}.{ext}",
            arch = match arch {
                zed::Architecture::Aarch64 => "arm64",
                zed::Architecture::X86 => "amd64",
                zed::Architecture::X8664 => "amd64",
            },
            os = match platform {
                zed::Os::Mac => "darwin",
                zed::Os::Linux => "linux",
                zed::Os::Windows => "windows",
            },
            ext = match platform {
                zed::Os::Windows => "zip",
                _ => "tar.gz",
            }
        );

        let file_type = match platform {
            zed::Os::Windows => zed::DownloadedFileType::Zip,
            _ => zed::DownloadedFileType::GzipTar,
        };

        let version_dir = format!("php-mago-{}", release.version);
        let bin_name = bin_name();
        let version_binary_path = format!("{version_dir}/{bin_name}");

        if !fs::metadata(&version_binary_path).map_or(false, |stat| stat.is_file()) {
            update_status(id, Status::Downloading);

            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name == asset_name)
                .ok_or_else(|| format!("no asset found matching {:?}", asset_name))?;
            zed::download_file(&asset.download_url, &version_dir, file_type)
                .map_err(|e| format!("failed to download file: {e}"))?;

            let entries =
                fs::read_dir(".").map_err(|e| format!("failed to list working directory {e}"))?;
            for entry in entries {
                let entry = entry.map_err(|e| format!("failed to load directory entry {e}"))?;
                if entry.file_name().to_str() != Some(&version_dir) {
                    fs::remove_dir_all(entry.path()).ok();
                }
            }

            update_status(id, Status::None);
        }

        Ok(version_binary_path)
    }
}

impl zed::Extension for MagoLspExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = self
            .language_server_binary_path(language_server_id, worktree)
            .inspect_err(|err| {
                update_status(language_server_id, Status::Failed(err.to_string()));
            })?;
        Ok(zed::Command {
            command: command,
            args: vec![],
            env: Default::default(),
        })
    }
}

zed::register_extension!(MagoLspExtension);
