use std::{
    collections::BTreeMap,
    sync::Arc,
};

use anyhow::Context as AnyhowContext;
use async_zip_0_0_9::{
    read::stream::ZipFileReader,
    write::ZipFileWriter,
    Compression,
    ZipEntryBuilder,
    ZipEntryBuilderExt,
};
use bytes::{
    Bytes,
    BytesMut,
};
use common::{
    sha256::{
        Sha256,
        Sha256Digest,
    },
    types::{
        ModuleEnvironment,
        ObjectKey,
    },
};
use futures::StreamExt;
use serde::{
    Deserialize,
    Serialize,
};
use storage::{
    ChannelWriter,
    Storage,
    StorageExt,
    Upload,
    UploadExt,
};
use sync_types::CanonicalizedModulePath;
use tokio::{
    io::{
        AsyncWrite,
        AsyncWriteExt,
    },
    sync::mpsc,
};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    config::types::{
        deprecated_extract_environment_from_path,
        ModuleConfig,
    },
    modules::module_versions::ModuleSource,
    source_packages::types::{
        PackageSize,
        MAX_ZIPPED_PACKAGES_SIZE,
    },
};

#[derive(Debug)]
pub struct PackagedFile {
    // TODO: or maybe we should store checksum + length in the module version metadata?
    pub file_checksum: Sha256Digest,
    pub source_map_checksum: Option<Sha256Digest>,
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct MetadataJson {
    module_paths: Vec<String>,
    module_environments: Option<Vec<(String, ModuleEnvironment)>>,
    external_deps_storage_key: Option<String>,
}

#[fastrace::trace]
async fn write_package(
    package: BTreeMap<CanonicalizedModulePath, &ModuleConfig>,
    mut out: impl AsyncWrite + Send + Unpin,
    external_deps_storage_key: Option<ObjectKey>,
) -> anyhow::Result<(usize, BTreeMap<CanonicalizedModulePath, PackagedFile>)> {
    let mut writer = ZipFileWriter::new(&mut out);
    let mut files = BTreeMap::new();
    let mut module_paths = vec![];
    let mut module_environments = Vec::new();
    let mut unzipped_size_bytes: usize = 0;
    for (path, module) in package {
        let source = module.source.as_bytes();
        // I would use Zstd since it is faster to decompress and gives similar
        // compression ratio. However, the node.js library fails with it. We can
        // easily change this later.
        let source_path = format!("modules/{}", String::from(path.clone()));
        // 0o644 => read-write for owner, read for everyone else.
        let builder =
            ZipEntryBuilder::new(source_path.clone(), Compression::Deflate).unix_permissions(0o644);
        module_paths.push(String::from(path.clone()));
        module_environments.push((String::from(path.clone()), module.environment));
        unzipped_size_bytes += source.len();
        writer.write_entry_whole(builder, source).await?;

        let file_checksum = Sha256::hash(source);
        let mut source_map_checksum = None;
        if let Some(ref source_map) = module.source_map {
            let source_map = source_map.as_bytes();
            // NB: All modules' canonicalized paths have a ".js" extension, so it's safe to
            // suffix this with ".map".
            let source_map_path = format!("modules/{}.map", String::from(path.clone()));
            let builder = ZipEntryBuilder::new(source_map_path.clone(), Compression::Deflate)
                .unix_permissions(0o644);
            module_paths.push(String::from(path.clone()) + ".map");
            unzipped_size_bytes += source_map.len();
            writer.write_entry_whole(builder, source_map).await?;

            source_map_checksum = Some(Sha256::hash(source_map));
        }

        let packaged_file = PackagedFile {
            file_checksum,
            source_map_checksum,
        };
        anyhow::ensure!(files.insert(path, packaged_file).is_none());
    }

    let metadata_entry = ZipEntryBuilder::new("metadata.json".to_string(), Compression::Deflate);
    let metadata_contents = MetadataJson {
        module_paths,
        module_environments: Some(module_environments),
        external_deps_storage_key: external_deps_storage_key.map(|key| key.to_string()),
    };
    let metadata_json = serde_json::to_vec(&metadata_contents)?;
    unzipped_size_bytes += metadata_json.len();
    writer
        .write_entry_whole(metadata_entry, &metadata_json)
        .await?;

    writer.close().await?;
    out.shutdown().await?;

    Ok((unzipped_size_bytes, files))
}

#[fastrace::trace]
pub async fn upload_package(
    package: BTreeMap<CanonicalizedModulePath, &ModuleConfig>,
    storage: Arc<dyn Storage>,
    external_deps_storage_key: Option<ObjectKey>,
) -> anyhow::Result<(ObjectKey, Sha256Digest, PackageSize)> {
    let (sender, receiver) = mpsc::channel::<Bytes>(1);
    let mut upload = storage.start_upload().await?;
    let uploader = upload.try_write_parallel_and_hash(ReceiverStream::new(receiver).map(Ok));
    let writer = ChannelWriter::new(sender, 5 * (1 << 20));
    let packager = write_package(package, writer, external_deps_storage_key);
    let ((unzipped_size_bytes, _packaged_files), (zipped_size_bytes, sha256)) =
        futures::try_join!(packager, uploader)?;
    let key = upload.complete().await?;
    Ok((
        key,
        sha256,
        PackageSize {
            zipped_size_bytes,
            unzipped_size_bytes,
        },
    ))
}

#[fastrace::trace]
pub async fn download_package(
    storage: Arc<dyn Storage>,
    key: ObjectKey,
    expected_digest: Sha256Digest,
) -> anyhow::Result<BTreeMap<CanonicalizedModulePath, ModuleConfig>> {
    let stream = storage
        .get(&key)
        .await?
        .context(format!("Src Pkg storage key not found?? {key:?}"))?;
    let storage::StorageGetStream {
        content_length,
        mut stream,
    } = stream;
    let expected_length: usize = content_length
        .try_into()
        .context("Source package content length cannot be negative")?;
    anyhow::ensure!(
        expected_length < MAX_ZIPPED_PACKAGES_SIZE,
        "Source package download is too large: {expected_length} bytes",
    );
    let mut downloaded = BytesMut::with_capacity(expected_length);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let downloaded_length = downloaded
            .len()
            .checked_add(chunk.len())
            .context("Source package download length overflow")?;
        anyhow::ensure!(
            downloaded_length < MAX_ZIPPED_PACKAGES_SIZE,
            "Source package download exceeded the maximum zipped size",
        );
        downloaded.extend_from_slice(&chunk);
    }
    anyhow::ensure!(
        downloaded.len() == expected_length,
        "Source package content length mismatch: expected {expected_length}, downloaded {}",
        downloaded.len(),
    );
    let downloaded = downloaded.freeze();
    let actual_digest = Sha256::hash(&downloaded);
    anyhow::ensure!(
        actual_digest == expected_digest,
        "Source package SHA-256 mismatch for {key:?}: expected {}, downloaded {}",
        expected_digest.as_hex(),
        actual_digest.as_hex(),
    );

    // Parse exactly the byte sequence whose digest was verified above. Keeping
    // the verified object in memory also avoids a check/use race if an object
    // store implementation permits replacement under the same key.
    let stream =
        futures::stream::once(async move { Ok::<Bytes, std::io::Error>(downloaded) }).boxed();
    let mut reader = ZipFileReader::new(
        storage::StorageGetStream {
            content_length,
            stream,
        }
        .into_tokio_reader(),
    );

    let mut source = BTreeMap::new();
    let mut source_maps = BTreeMap::new();

    let mut metadata_json: Option<MetadataJson> = None;
    while let Some(entry_reader) = reader.entry_reader().await? {
        let entry = entry_reader.entry();
        let path = entry.filename().to_string();
        let contents = entry_reader.read_to_string_crc().await?;

        if path == "metadata.json" {
            metadata_json = Some(serde_json::from_str(&contents)?);
            continue;
        }

        let path = path
            .strip_prefix("modules/")
            .context("Path does not start with modules/?")?;
        let (module_path, is_source_map) = if path.ends_with(".js") {
            (path.parse::<CanonicalizedModulePath>()?, false)
        } else if path.ends_with(".js.map") {
            (path.trim_end_matches(".map").parse()?, true)
        } else {
            anyhow::bail!("Invalid path in archive: {path}");
        };
        if is_source_map {
            source_maps.insert(module_path, contents);
        } else {
            source.insert(module_path, contents);
        }
    }
    // Drain the rest of the reader until it reaches the central directory entry,
    // even if we've already hit the last entry.
    while !reader.finished() {
        anyhow::ensure!(reader.entry_reader().await?.is_none());
    }

    // Make sure metadata.json looks right
    let metadata_json = metadata_json.context("metadata.json not found")?;

    let mut found_paths: Vec<_> = source
        .keys()
        .map(|k| k.clone().into())
        .chain(source_maps.keys().map(|k| String::from(k.clone()) + ".map"))
        .collect();
    found_paths.sort();
    let mut metadata_paths = metadata_json.module_paths.clone();
    metadata_paths.sort();
    anyhow::ensure!(
        metadata_paths == found_paths,
        "metadata.json paths don't match paths in zip for source package {key:?}",
    );

    let mut module_environments: Option<BTreeMap<String, ModuleEnvironment>> = metadata_json
        .module_environments
        .map(|module_environments| module_environments.into_iter().collect());

    let mut out = BTreeMap::new();
    for (path, source) in source {
        // If the module_environments is missing, we default to using the path.
        // Otherwise, the module must be present.
        let environment = match module_environments.as_mut() {
            Some(module_environments) => module_environments
                .remove(&String::from(path.clone()))
                .ok_or_else(|| anyhow::anyhow!("Missing environment for module: {path:?}")),
            None => deprecated_extract_environment_from_path(path.clone().into()),
        }?;
        let config = ModuleConfig {
            path: path.clone().into(),
            source: ModuleSource::new(&source),
            source_map: source_maps.remove(&path),
            environment,
        };
        out.insert(path, config);
    }
    Ok(out)
}
