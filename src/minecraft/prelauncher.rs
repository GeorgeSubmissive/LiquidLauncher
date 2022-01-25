use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::Result;
use log::*;

use crate::cloud::{Build, LauncherApi, LaunchManifest, LoaderSubsystem, LoaderVersion, ModSource};
use crate::error::LauncherError;
use crate::interface::webviews::download_client;
use crate::minecraft::launcher;
use crate::minecraft::launcher::{LauncherData, LaunchingParameter};
use crate::minecraft::progress::{get_max, get_progress, ProgressReceiver, ProgressUpdate, ProgressUpdateSteps};
use crate::minecraft::version::{VersionManifest, VersionProfile};
use crate::utils::{download_file, get_maven_artifact_path};

///
/// Prelaunching client
///
pub(crate) async fn launch<D: Send + Sync>(build: &Build, launching_parameter: LaunchingParameter, launcher_data: LauncherData<D>) -> Result<()> {
    info!("Loading minecraft version manifest...");
    let mc_version_manifest = VersionManifest::download().await?;

    info!("Loading launch manifest...");
    let launch_manifest = LauncherApi::load_version_manifest(build.build_id).await?;
    let loader = &launch_manifest.loader;

    launcher_data.progress_update(ProgressUpdate::set_max());
    launcher_data.progress_update(ProgressUpdate::SetProgress(0));

    // Copy retrieve and copy mods from manifest
    retrieve_and_copy_mods(&launch_manifest, &launcher_data).await?;

    info!("Loading version profile...");
    let manifest_url = match loader.subsystem {
        LoaderSubsystem::Fabric => loader.launcher_manifest
            .replace("{MINECRAFT_VERSION}", &*build.mc_version)
            .replace("{FABRIC_LOADER_VERSION}", &*build.fabric_loader_version),
        LoaderSubsystem::Forge => loader.launcher_manifest.clone()
    };
    let mut version = VersionProfile::load(&manifest_url).await?;

    if let Some(inherited_version) = &version.inherits_from {
        let url = mc_version_manifest.versions
            .iter()
            .find(|x| &x.id == inherited_version)
            .map(|x| &x.url)
            .ok_or_else(|| LauncherError::InvalidVersionProfile(format!("unable to find inherited version manifest {}", inherited_version)))?;

        debug!("Determined {}'s download url to be {}", inherited_version, url);
        info!("Downloading inherited version {}...", inherited_version);

        let parent_version = VersionProfile::load(url).await?;

        version.merge(parent_version)?;
    }

    info!("Launching {}...", launch_manifest.build.commit_id);

    launcher::launch(version, launching_parameter, launcher_data).await?;
    Ok(())
}

pub(crate) async fn retrieve_and_copy_mods(manifest: &LaunchManifest, progress: &impl ProgressReceiver) -> anyhow::Result<()> {
    let mod_cache_path = Path::new("mod_cache");
    let mods_path = Path::new("gameDir").join("mods");

    tokio::fs::create_dir_all(&mod_cache_path).await?;
    tokio::fs::create_dir_all(&mods_path).await?;

    // Clear mods directory
    for x in std::fs::read_dir(&mods_path)? {
        let entry = x?;

        if entry.file_type()?.is_file() {
            std::fs::remove_file(entry.path())?;
        }
    }

    let max = get_max(manifest.mods.len());

    for (mod_idx, current_mod) in manifest.mods.iter().enumerate() {
        // Skip mods that are not needed
        if !current_mod.required && !current_mod.default {
            continue;
        }

        progress.progress_update(ProgressUpdate::set_label(format!("Downloading recommended mod {}", current_mod.name)));

        let current_mod_path = mod_cache_path.join(current_mod.source.get_path()?);

        // Do we need to download the mod?
        if !current_mod_path.exists() {
            // Make sure that the parent directory exists
            tokio::fs::create_dir_all(&current_mod_path.parent().unwrap()).await?;

            match &current_mod.source {
                ModSource::SkipAd { artifact_name, url, extract } => {
                    let retrieved_bytes = download_client(url, |a, b| progress.progress_update(ProgressUpdate::set_for_step(ProgressUpdateSteps::DownloadLiquidBounceMods, get_progress(mod_idx, a, b) as u64, max))).await?;

                    // Extract bytes
                    let final_file = if *extract {
                        let mut archive = zip::ZipArchive::new(Cursor::new(retrieved_bytes)).unwrap();

                        let file_name_to_extract = archive.file_names().find(|x| x.ends_with(".jar")).ok_or_else(|| LauncherError::InvalidVersionProfile(format!("There is no JAR in the downloaded archive")))?.to_owned();

                        let mut file_to_extract = archive.by_name(&file_name_to_extract)?;

                        let mut output = Vec::with_capacity(file_to_extract.size() as usize);

                        file_to_extract.read_to_end(&mut output)?;

                        output
                    } else {
                        retrieved_bytes
                    };

                    tokio::fs::write(&current_mod_path, final_file).await?;
                },
                ModSource::Repository { repository, artifact } => {
                    info!("downloading mod {} from {}", artifact, repository);
                    let repository_url = manifest.repositories.get(repository).ok_or_else(|| LauncherError::InvalidVersionProfile(format!("There is no repository specified with the name {}", repository)))?;

                    let retrieved_bytes = download_file(&format!("{}{}", repository_url, get_maven_artifact_path(artifact)?), |a, b| {
                        progress.progress_update(ProgressUpdate::set_for_step(ProgressUpdateSteps::DownloadLiquidBounceMods, get_progress(mod_idx, a, b), max));
                    }).await?;

                    tokio::fs::write(&current_mod_path, retrieved_bytes).await?;
                }
            }
        }

        // Copy the mod.
        tokio::fs::copy(&current_mod_path, mods_path.join(format!("{}.jar", current_mod.name))).await?;
    }

    Ok(())

}