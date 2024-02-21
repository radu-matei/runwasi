#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;

use containerd_client;
use containerd_client::services::v1::containers_client::ContainersClient;
use containerd_client::services::v1::content_client::ContentClient;
use containerd_client::services::v1::images_client::ImagesClient;
use containerd_client::services::v1::leases_client::LeasesClient;
use containerd_client::services::v1::{
    Container, DeleteContentRequest, GetContainerRequest, GetImageRequest, Image, Info,
    InfoRequest, ReadContentRequest, UpdateImageRequest, UpdateRequest, WriteAction,
    WriteContentRequest,
};
use containerd_client::tonic::transport::Channel;
use containerd_client::{tonic, with_namespace};
use futures::TryStreamExt;
use oci_spec::image::{Arch, ImageManifest, MediaType, Platform};
use prost_types::FieldMask;
use sha256::digest;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request};

use super::lease::LeaseGuard;
use crate::container::Engine;
use crate::sandbox::error::{Error as ShimError, Result};
use crate::sandbox::oci::{self, WasmLayer};
use crate::with_lease;

static PRECOMPILE_PREFIX: &str = "runwasi.io/precompiled";

pub struct Client {
    inner: Channel,
    rt: Runtime,
    namespace: String,
    address: String,
}

#[derive(Debug)]
pub(crate) struct WriteContent {
    _lease: LeaseGuard,
    pub digest: String,
}

// sync wrapper implementation from https://tokio.rs/tokio/topics/bridging
impl Client {
    // wrapper around connection that will establish a connection and create a client
    pub fn connect(
        address: impl AsRef<Path> + ToString,
        namespace: impl ToString,
    ) -> Result<Client> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let inner = rt
            .block_on(containerd_client::connect(address.as_ref()))
            .map_err(|err| ShimError::Containerd(err.to_string()))?;

        Ok(Client {
            inner,
            rt,
            namespace: namespace.to_string(),
            address: address.to_string(),
        })
    }

    // wrapper around read that will read the entire content file
    fn read_content(&self, digest: impl ToString) -> Result<Vec<u8>> {
        self.rt.block_on(async {
            let req = ReadContentRequest {
                digest: digest.to_string(),
                ..Default::default()
            };
            let req = with_namespace!(req, self.namespace);
            ContentClient::new(self.inner.clone())
                .read(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?
                .into_inner()
                .map_ok(|msg| msg.data)
                .try_concat()
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))
        })
    }

    // used in tests to clean up content
    #[allow(dead_code)]
    fn delete_content(&self, digest: impl ToString) -> Result<()> {
        self.rt.block_on(async {
            let req = DeleteContentRequest {
                digest: digest.to_string(),
            };
            let req = with_namespace!(req, self.namespace);
            ContentClient::new(self.inner.clone())
                .delete(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?;
            Ok(())
        })
    }

    // wrapper around lease that will create a lease and return a guard that will delete the lease when dropped
    fn lease(&self, reference: String) -> Result<LeaseGuard> {
        self.rt.block_on(async {
            let mut lease_labels = HashMap::new();
            let expire = chrono::Utc::now() + chrono::Duration::hours(24);
            lease_labels.insert("containerd.io/gc.expire".to_string(), expire.to_rfc3339());
            let lease_request = containerd_client::services::v1::CreateRequest {
                id: reference.clone(),
                labels: lease_labels,
            };

            let mut leases_client = LeasesClient::new(self.inner.clone());

            let lease = leases_client
                .create(with_namespace!(lease_request, self.namespace))
                .await
                .map_err(|e| ShimError::Containerd(e.to_string()))?
                .into_inner()
                .lease
                .ok_or_else(|| {
                    ShimError::Containerd(format!("unable to create lease for  {}", reference))
                })?;

            Ok(LeaseGuard {
                lease_id: lease.id,
                address: self.address.clone(),
                namespace: self.namespace.clone(),
            })
        })
    }

    fn save_content(
        &self,
        data: Vec<u8>,
        original_digest: String,
        label: &str,
    ) -> Result<WriteContent> {
        let expected = format!("sha256:{}", digest(data.clone()));
        let reference = format!("precompile-{}", label);
        let lease = self.lease(reference.clone())?;

        let digest = self.rt.block_on(async {
            // create a channel to feed the stream; only sending one message at a time so we can set this to one
            let (tx, rx) = mpsc::channel(1);

            let len = data.len() as i64;
            log::debug!("Writing {} bytes to content store", len);
            let mut client = ContentClient::new(self.inner.clone());

            // Send write request with Stat action to containerd to let it know that we are going to write content
            // if the content is already there, it will return early with AlreadyExists
            log::debug!("Sending stat request to containerd");
            let req = WriteContentRequest {
                r#ref: reference.clone(),
                action: WriteAction::Stat.into(),
                total: len,
                expected: expected.clone(),
                ..Default::default()
            };
            tx.send(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?;
            let request_stream = ReceiverStream::new(rx);
            let request_stream =
                with_lease!(request_stream, self.namespace, lease.lease_id.clone());
            let mut response_stream = match client.write(request_stream).await {
                Ok(response_stream) => response_stream.into_inner(),
                Err(e) if e.code() == Code::AlreadyExists => {
                    log::info!("content already exists {}", expected.clone().to_string());
                    return Ok(expected);
                }
                Err(e) => return Err(ShimError::Containerd(e.to_string())),
            };
            let response = response_stream
                .message()
                .await
                .map_err(|e| ShimError::Containerd(e.to_string()))?
                .ok_or_else(|| {
                    ShimError::Containerd(format!(
                        "no response received after write request for {}",
                        expected
                    ))
                })?;

            // There is a scenario where the content might have been removed manually
            // but the content isn't removed from the containerd file system yet.
            // In this case if we re-add it at before its removed from file system
            // we don't need to copy the content again.  Container tells us it found the blob
            // by returning the offset of the content that was found.
            let data_to_write = data[response.offset as usize..].to_vec();

            // Write and commit at same time
            let mut labels = HashMap::new();
            labels.insert(label.to_string(), original_digest.clone());
            let commit_request = WriteContentRequest {
                action: WriteAction::Commit.into(),
                total: len,
                offset: response.offset,
                expected: expected.clone(),
                labels,
                data: data_to_write,
                ..Default::default()
            };
            log::debug!(
                "Sending commit request to containerd with response: {:?}",
                response
            );
            tx.send(commit_request)
                .await
                .map_err(|err| ShimError::Containerd(format!("commit request error: {}", err)))?;
            let response = response_stream
                .message()
                .await
                .map_err(|err| ShimError::Containerd(format!("response stream error: {}", err)))?
                .ok_or_else(|| {
                    ShimError::Containerd(format!(
                        "no response received after write request for {}",
                        expected.clone()
                    ))
                })?;

            log::debug!("Validating response");
            // client should validate that all bytes were written and that the digest matches
            if response.offset != len {
                return Err(ShimError::Containerd(format!(
                    "failed to write all bytes, expected {} got {}",
                    len, response.offset
                )));
            }
            if response.digest != expected {
                return Err(ShimError::Containerd(format!(
                    "unexpected digest, expected {} got {}",
                    expected, response.digest
                )));
            }
            Ok(response.digest)
        })?;

        Ok(WriteContent {
            _lease: lease,
            digest: digest.clone(),
        })
    }

    fn get_info(&self, content_digest: String) -> Result<Info> {
        self.rt.block_on(async {
            let req = InfoRequest {
                digest: content_digest.clone(),
            };
            let req = with_namespace!(req, self.namespace);
            let info = ContentClient::new(self.inner.clone())
                .info(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?
                .into_inner()
                .info
                .ok_or_else(|| {
                    ShimError::Containerd(format!(
                        "failed to get info for content {}",
                        content_digest
                    ))
                })?;
            Ok(info)
        })
    }

    fn update_info(&self, info: Info) -> Result<Info> {
        self.rt.block_on(async {
            let req = UpdateRequest {
                info: Some(info.clone()),
                update_mask: Some(FieldMask {
                    paths: vec!["labels".to_string()],
                }),
            };
            let req = with_namespace!(req, self.namespace);
            let info = ContentClient::new(self.inner.clone())
                .update(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?
                .into_inner()
                .info
                .ok_or_else(|| {
                    ShimError::Containerd(format!(
                        "failed to update info for content {}",
                        info.digest
                    ))
                })?;
            Ok(info)
        })
    }

    fn get_image(&self, image_name: impl ToString) -> Result<Image> {
        self.rt.block_on(async {
            let name = image_name.to_string();
            let req = GetImageRequest { name };
            let req = with_namespace!(req, self.namespace);
            let image = ImagesClient::new(self.inner.clone())
                .get(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?
                .into_inner()
                .image
                .ok_or_else(|| {
                    ShimError::Containerd(format!(
                        "failed to get image for image {}",
                        image_name.to_string()
                    ))
                })?;
            Ok(image)
        })
    }

    fn update_image(&self, image: Image) -> Result<Image> {
        self.rt.block_on(async {
            let req = UpdateImageRequest {
                image: Some(image.clone()),
                update_mask: Some(FieldMask {
                    paths: vec!["labels".to_string()],
                }),
            };

            let req = with_namespace!(req, self.namespace);
            let image = ImagesClient::new(self.inner.clone())
                .update(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?
                .into_inner()
                .image
                .ok_or_else(|| {
                    ShimError::Containerd(format!("failed to update image {}", image.name))
                })?;
            Ok(image)
        })
    }

    fn extract_image_content_sha(&self, image: &Image) -> Result<String> {
        let digest = image
            .target
            .as_ref()
            .ok_or_else(|| {
                ShimError::Containerd(format!(
                    "failed to get image content sha for image {}",
                    image.name
                ))
            })?
            .digest
            .clone();
        Ok(digest)
    }

    fn get_container(&self, container_name: impl ToString) -> Result<Container> {
        self.rt.block_on(async {
            let id = container_name.to_string();
            let req = GetContainerRequest { id };
            let req = with_namespace!(req, self.namespace);
            let container = ContainersClient::new(self.inner.clone())
                .get(req)
                .await
                .map_err(|err| ShimError::Containerd(err.to_string()))?
                .into_inner()
                .container
                .ok_or_else(|| {
                    ShimError::Containerd(format!(
                        "failed to get image for container {}",
                        container_name.to_string()
                    ))
                })?;
            Ok(container)
        })
    }

    pub fn load_components<T: Engine>(
        &self,
        containerd_id: impl ToString,
        engine: &T,
    ) -> Result<(Vec<oci::WasmLayer>, Platform)> {
        let container = self.get_container(containerd_id.to_string())?;
        let mut image = self.get_image(container.image)?;
        log::info!("    xxx SHIM: image: {:?}", image.name);

        let manifest = ImageManifest::from_reader(
            self.read_content(self.extract_image_content_sha(&image)?)?
                .as_slice(),
        )?;

        let image_config = self.read_content(manifest.config().digest())?;

        // the only part we care about here is the platform values
        let platform: Platform = serde_json::from_slice(&image_config)?;
        let Arch::Wasm = platform.architecture() else {
            log::info!("manifest is not in WASM OCI image format");
            return Ok((vec![], platform));
        };

        log::info!("found manifest with WASM OCI image format.");
        let mut res = Vec::new();

        // At this point, this is definitely an OCI reference that contains Wasm. Proposed
        // algorithm:
        //  * collect the supported layers
        //  * check if any layer is already precompiled (is a label for it present in the
        //  containerd content store?) (additional optimization of only checking actual wasm
        //  content, not static assets; not easily generalizable for other shims)
        //  * if yes, add it to the final layer slice
        //  * if no, add it to a layer slice that needs to be precompiled
        //  * precompile all layers that need it
        //  * add them to the containerd content store, with labels and GC refs
        //  * return
        //
        //  An issue here for Spin apps is that we want two things at the same time from runwasi:
        //      * runwasi to return *all* layers present in an OCI image
        //      * only precompile and store wasm content
        //  This seems difficult with the supported_layers_types function from the engine trait.

        for cfg in manifest.layers().clone() {
            log::trace!("      <<< Layer: {:?}: {}", cfg.media_type(), cfg.digest());
            if is_supported_layer(cfg.media_type(), T::supported_layers_types()) {
                res.push(WasmLayer {
                    config: cfg.clone(),
                    layer: self.read_content(cfg.digest())?,
                });
            }
        }

        Ok((res, platform))
    }

    // load module will query the containerd store to find an image that has an OS of type 'wasm'
    // If found it continues to parse the manifest and return the layers that contains the WASM modules
    // and possibly other configuration layers.
    pub fn load_modules<T: Engine>(
        &self,
        containerd_id: impl ToString,
        engine: &T,
    ) -> Result<(Vec<oci::WasmLayer>, Platform)> {
        let container = self.get_container(containerd_id.to_string())?;
        let mut image = self.get_image(container.image)?;
        log::info!("    xxx SHIM: image: {:?}", image.name);
        let image_digest = self.extract_image_content_sha(&image)?;
        let manifest = self.read_content(image_digest.clone())?;
        let manifest = manifest.as_slice();
        let manifest = ImageManifest::from_reader(manifest)?;

        let image_config_descriptor = manifest.config();
        let image_config = self.read_content(image_config_descriptor.digest())?;
        let image_config = image_config.as_slice();

        // the only part we care about here is the platform values
        let platform: Platform = serde_json::from_slice(image_config)?;
        let Arch::Wasm = platform.architecture() else {
            log::info!("manifest is not in WASM OCI image format");
            return Ok((vec![], platform));
        };

        log::info!("found manifest with WASM OCI image format.");
        // This label is unique across runtimes and version of the shim running
        // a precompiled component/module will not work across different runtimes or versions
        let (can_precompile, precompile_id) = match engine.can_precompile() {
            Some(precompile_id) => (true, precompile_label(T::name(), &precompile_id)),
            None => (false, "".to_string()),
        };

        match image.labels.get(&precompile_id) {
            Some(precompile_digest) if can_precompile => {
                log::info!("found precompiled label: {} ", &precompile_id);
                match self.read_content(precompile_digest) {
                    Ok(precompiled) => {
                        log::info!("found precompiled module in cache: {} ", &precompile_digest);
                        return Ok((
                            vec![WasmLayer {
                                config: image_config_descriptor.clone(),
                                layer: precompiled,
                            }],
                            platform,
                        ));
                    }
                    Err(e) => {
                        // log and continue
                        log::warn!("failed to read precompiled module from cache: {}. Content may have been removed manually, will attempt to recompile", e);
                    }
                }
            }
            _ => {}
        }

        for l in manifest.layers().clone() {
            log::info!(
                "                   XXX SHIM: {:?}: {}",
                l.media_type(),
                l.digest()
            );
        }

        let layers = manifest
            .layers()
            .iter()
            .filter(|x| is_supported_layer(x.media_type(), T::supported_layers_types()))
            .map(|config| self.read_content(config.digest()))
            .collect::<Result<Vec<_>>>()?;

        if layers.is_empty() {
            log::info!("no WASM modules found in OCI layers");
            return Ok((vec![], platform));
        }

        if can_precompile {
            log::info!("precompiling module");
            let precompiled = engine.precompile(layers.as_slice())?;
            log::info!("precompiling module: {}", image_digest.clone());
            let precompiled_content =
                self.save_content(precompiled.clone(), image_digest.clone(), &precompile_id)?;

            log::debug!("updating image with compiled content digest");
            image
                .labels
                .insert(precompile_id, precompiled_content.digest.clone());
            self.update_image(image)?;

            // The original image is considered a root object, by adding a ref to the new compiled content
            // We tell containerd to not garbage collect the new content until this image is removed from the system
            // this ensures that we keep the content around after the lease is dropped
            log::debug!("updating content with precompile digest to avoid garbage collection");
            let mut image_content = self.get_info(image_digest.clone())?;
            image_content.labels.insert(
                "containerd.io/gc.ref.content.precompile".to_string(),
                precompiled_content.digest.clone(),
            );
            self.update_info(image_content)?;

            return Ok((
                vec![WasmLayer {
                    config: image_config_descriptor.clone(),
                    layer: precompiled,
                }],
                platform,
            ));
        }

        log::info!("using module from OCI layers");
        let layers = layers
            .into_iter()
            .map(|module| WasmLayer {
                config: image_config_descriptor.clone(),
                layer: module,
            })
            .collect::<Vec<_>>();
        Ok((layers, platform))
    }
}

fn precompile_label(name: &str, version: &str) -> String {
    format!("{}/{}/{}", PRECOMPILE_PREFIX, name, version)
}

fn is_supported_layer(media_type: &MediaType, supported_layer_types: &[&str]) -> bool {
    supported_layer_types.contains(&media_type.to_string().as_str())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_save_content() {
        let path = PathBuf::from("/run/containerd/containerd.sock");
        let path = path.to_str().unwrap();
        let client = Client::connect(path, "test-ns").unwrap();
        let data = b"hello world".to_vec();

        let expected = digest(data.clone());
        let expected = format!("sha256:{}", expected);

        let label = precompile_label("test", "hasdfh");
        let returned = client
            .save_content(data, "original".to_string(), &label)
            .unwrap();
        assert_eq!(expected, returned.digest.clone());

        let data = client.read_content(returned.digest.clone()).unwrap();
        assert_eq!(data, b"hello world");

        client
            .save_content(data.clone(), "original".to_string(), &label)
            .expect_err("Should not be able to save when lease is open");

        // need to drop the lease to be able to create a second one
        // a second call should be successful since it already exists
        drop(returned);

        // a second call should be successful since it already exists
        let returned = client
            .save_content(data, "original".to_string(), &label)
            .unwrap();
        assert_eq!(expected, returned.digest);

        client.delete_content(expected.clone()).unwrap();

        client
            .read_content(expected)
            .expect_err("content should not exist");
    }
}
