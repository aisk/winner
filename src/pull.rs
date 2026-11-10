//! Image pull. Async is confined to this module: a current-thread tokio
//! runtime drives oci-client, everything else in the program is sync.

use crate::image::{ImageRecord, normalize};
use crate::{host, paths, store};
use anyhow::{Context, Result, bail};
use oci_client::client::ClientConfig;
use oci_client::manifest::{ImageIndexEntry, OciDescriptor};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};

/// Pull an image and import its layers. Progress goes to stderr.
pub fn pull(reference: &str) -> Result<ImageRecord> {
    let reference = &normalize(reference)?;
    // Layer import needs these; fail before downloading hundreds of MB.
    crate::backup::enable_privileges(&["SeBackupPrivilege", "SeRestorePrivilege"])?;
    let _lock = store::Lock::shared()?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let record = rt.block_on(fetch(reference))?;
    store::import(&record.layers)?;
    record.save()?;
    Ok(record)
}

/// Pick the manifest whose os.version best matches the host build: an exact
/// build match first, otherwise the newest build not newer than the host.
fn windows_resolver(entries: &[ImageIndexEntry]) -> Option<String> {
    let host = host::build();
    entries
        .iter()
        .filter_map(|e| {
            let p = e.platform.as_ref()?;
            let is_target = p.os.to_string() == "windows" && p.architecture.to_string() == "amd64";
            let build = p.os_version.as_deref().and_then(host::build_of)?;
            (is_target && build <= host).then_some((build, e))
        })
        .max_by_key(|(build, _)| (*build == host, *build))
        .map(|(_, e)| e.digest.clone())
}

async fn fetch(reference: &str) -> Result<ImageRecord> {
    let image: Reference = reference.parse().context("invalid image reference")?;
    let client = Client::new(ClientConfig {
        platform_resolver: Some(Box::new(windows_resolver)),
        ..Default::default()
    });
    // TODO: credentials from ~/.docker/config.json for private registries.
    let auth = RegistryAuth::Anonymous;

    let (manifest, digest, config) = client
        .pull_manifest_and_config(&image, &auth)
        .await
        .context("pull manifest")?;
    let config: serde_json::Value = serde_json::from_str(&config)?;
    if config["os"] != "windows" {
        bail!("{reference} is not a Windows image");
    }

    // Download layers concurrently on the single-threaded runtime.
    let mut tasks = tokio::task::JoinSet::new();
    for desc in manifest.layers.clone() {
        let (client, image) = (client.clone(), image.clone());
        tasks.spawn(async move { download(&client, &image, &desc).await });
    }
    while let Some(r) = tasks.join_next().await {
        r??;
    }

    Ok(ImageRecord {
        reference: reference.to_string(),
        manifest_digest: digest,
        os_version: config["os.version"].as_str().map(Into::into),
        layers: manifest.layers.iter().map(|l| l.digest.clone()).collect(),
        config,
    })
}

async fn download(client: &Client, image: &Reference, desc: &OciDescriptor) -> Result<()> {
    let dest = paths::blob(&desc.digest);
    if dest.exists() {
        return Ok(());
    }
    // TODO: foreign layers (`...rootfs.foreign.diff.tar.gzip`) must be fetched
    // from `desc.urls` instead of the registry.
    std::fs::create_dir_all(dest.parent().unwrap())?;
    // Per process: concurrent pulls of the same blob must not share a file.
    let tmp = paths::partial_blob(&desc.digest);
    let file = tokio::fs::File::create(&tmp).await?;
    eprintln!("downloading {} ({} MB)", desc.digest, desc.size / 1024 / 1024);
    // pull_blob verifies the digest while streaming.
    client
        .pull_blob(image, desc, file)
        .await
        .with_context(|| format!("download {}", desc.digest))?;
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}
