/*
 * This file is part of Edgehog.
 *
 * Copyright 2022 SECO Mind Srl
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use std::collections::HashMap;

use astarte_sdk::types::AstarteType;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use zbus::export::futures_util::StreamExt;

use crate::error::DeviceManagerError;
use crate::power_management;
use crate::rauc::RaucProxy;

#[derive(Serialize, Deserialize, Debug)]
struct PersistentState {
    uuid: String,
    slot: String,
}

#[derive(thiserror::Error, Debug)]
pub enum OTAError {
    /// Bundle download failed
    #[error("OTAErrorNetwork")]
    Network,
    /// Deployment failed
    #[error("OTAErrorDeploy")]
    Deploy,
    /// OTA failed
    #[error("OTAFailed")]
    Failed,
}

#[derive(Debug)]
enum OTAStatus {
    InProgress,
    Done,
    Error(OTAError),
}

impl OTAStatus {
    fn to_status_code(&self) -> (String, String) {
        match self {
            OTAStatus::InProgress => ("InProgress".to_string(), String::new()),
            OTAStatus::Done => ("Done".to_string(), String::new()),
            OTAStatus::Error(error) => ("Error".to_string(), error.to_string()),
        }
    }
}

pub struct OTAHandler<'a> {
    rauc: RaucProxy<'a>,
    state_file_path: String,
    download_file_path: String,
}

impl<'a> OTAHandler<'a> {
    pub async fn new(
        opts: &crate::DeviceManagerOptions,
    ) -> Result<OTAHandler<'a>, DeviceManagerError> {
        let connection = zbus::Connection::system().await?;

        let proxy = RaucProxy::new(&connection).await?;

        info!("boot slot = {:?}", proxy.boot_slot().await);
        info!("primary slot = {:?}", proxy.get_primary().await);

        Ok(OTAHandler {
            rauc: proxy,
            state_file_path: opts.state_file_path.clone(),
            download_file_path: opts.download_file_path.clone(),
        })
    }

    pub async fn last_error(&self) -> Result<String, DeviceManagerError> {
        Ok(self.rauc.last_error().await?)
    }

    pub async fn ota_event(
        &mut self,
        sdk: &astarte_sdk::AstarteSdk,
        data: HashMap<String, AstarteType>,
    ) -> Result<(), DeviceManagerError> {
        if let (AstarteType::String(request_url), AstarteType::String(request_uuid)) =
            (&data["url"], &data["uuid"])
        {
            let result = self.handle_ota_event(sdk, request_url, request_uuid).await;

            if let Err(err) = result {
                error!("Update failed!");
                error!("{:?}", err);
                error!("{:?}", self.last_error().await);

                match err {
                    DeviceManagerError::OTAError(err) => self
                        .send_ota_response(sdk, request_uuid, OTAStatus::Error(err))
                        .await
                        .ok(),
                    _ => self
                        .send_ota_response(sdk, request_uuid, OTAStatus::Error(OTAError::Failed))
                        .await
                        .ok(),
                };
            }
        } else {
            error!("Got bad data in OTARequest ({data:?})");
        }

        Ok(())
    }

    async fn handle_ota_event(
        &mut self,
        sdk: &astarte_sdk::AstarteSdk,
        request_url: &str,
        request_uuid: &str,
    ) -> Result<(), DeviceManagerError> {
        info!("Got update event");

        self.send_ota_response(sdk, request_uuid, OTAStatus::InProgress)
            .await?;

        let path = std::path::Path::new(&self.download_file_path).join("update.bin");
        let path = path.to_str().ok_or_else(|| {
            DeviceManagerError::FatalError("wrong download file path".to_string())
        })?;

        wget(request_url, path).await?;

        let bundle_info = self.rauc.info(path).await?;
        debug!("bundle info: {:?}", bundle_info);

        let compatible = self.rauc.compatible().await?;

        if bundle_info.compatible != compatible {
            error!(
                "bundle '{}' is not compatible with system '{}'",
                bundle_info.compatible, compatible
            );
            return Err(DeviceManagerError::UpdateError(
                "bundle is not compatible".to_owned(),
            ));
        }

        self.write_state_json(PersistentState {
            uuid: request_uuid.to_string(),
            slot: self.rauc.boot_slot().await?,
        })?;

        self.rauc
            .install_bundle(path, std::collections::HashMap::new())
            .await?;

        debug!(
            "install_bundle done, last_error={:?}",
            self.last_error().await
        );

        debug!("rauc operation = {}", self.rauc.operation().await?);

        let mut completed_update = self.rauc.receive_completed().await?;

        info!("Waiting for signal...");
        if let Some(completed) = completed_update.next().await {
            let signal = completed.args().unwrap();
            let signal = *signal.result();

            info!("Completed signal! {:?}", signal);

            match signal {
                0 => {
                    info!("Update successful");
                    info!("Rebooting in 5 seconds");

                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                    power_management::reboot()?;
                }
                _ => {
                    error!("Update failed with signal {signal}");
                    return Err(OTAError::Deploy.into());
                }
            }
        }

        Ok(())
    }

    pub async fn ensure_pending_ota_response(
        &self,
        sdk: &astarte_sdk::AstarteSdk,
    ) -> Result<(), DeviceManagerError> {
        if std::path::Path::new(&self.state_file_path).exists() {
            info!("Found pending update");
            let state = self.read_state_json()?;

            if state.slot != self.rauc.boot_slot().await? {
                info!("OTA successful");
                self.send_ota_response(sdk, &state.uuid, OTAStatus::Done)
                    .await?;
            } else {
                warn!("OTA failed");
                self.send_ota_response(sdk, &state.uuid, OTAStatus::Error(OTAError::Failed))
                    .await?;
            }

            std::fs::remove_file(&self.state_file_path)?;
        }

        Ok(())
    }

    async fn send_ota_response(
        &self,
        sdk: &astarte_sdk::AstarteSdk,
        request_uuid: &str,
        status: OTAStatus,
    ) -> Result<(), DeviceManagerError> {
        info!("Sending ota response {:?}", status);

        let (status, status_code) = status.to_status_code();

        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct OTAResponse {
            uuid: String,
            status: String,
            status_code: String,
        }

        sdk.send_object(
            "io.edgehog.devicemanager.OTAResponse",
            "/response",
            OTAResponse {
                uuid: request_uuid.to_string(),
                status,
                status_code,
            },
        )
        .await?;

        Ok(())
    }

    fn read_state_json(&self) -> Result<PersistentState, DeviceManagerError> {
        let str = std::fs::read_to_string(&self.state_file_path)?;

        let state: PersistentState = serde_json::from_str(&str)?;

        Ok(state)
    }

    fn write_state_json(&self, state: PersistentState) -> Result<(), DeviceManagerError> {
        let mut file = std::fs::File::create(&self.state_file_path)?;
        std::io::Write::write_all(&mut file, serde_json::to_string(&state)?.as_bytes())?;

        Ok(())
    }
}

async fn wget(url: &str, file_path: &str) -> Result<(), DeviceManagerError> {
    info!("Downloading {:?}", url);
    for i in 0..5 {
        let response = reqwest::get(url).await;

        match response {
            Ok(response) => {
                debug!("Writing {file_path}");
                let mut os_file = std::fs::File::create(&file_path)?;
                let mut content = std::io::Cursor::new(response.bytes().await?);
                std::io::copy(&mut content, &mut os_file)?;
                return Ok(());
            }
            Err(err) => {
                let wait = u64::pow(2, i);
                error!("Error downloading update: {err:?}");
                error!("Next attempt in {}s", wait);
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
        }
    }

    Err(OTAError::Network.into())
}

#[cfg(test)]
mod tests {
    use crate::ota_handler::{OTAError, OTAStatus};

    #[test]
    fn ota_status() {
        assert_eq!(
            ("Error".to_owned(), "OTAFailed".to_owned()),
            OTAStatus::Error(OTAError::Failed).to_status_code()
        );
        assert_eq!(
            ("Error".to_owned(), "OTAErrorNetwork".to_owned()),
            OTAStatus::Error(OTAError::Network).to_status_code()
        );
        assert_eq!(
            ("Error".to_owned(), "OTAErrorDeploy".to_owned()),
            OTAStatus::Error(OTAError::Deploy).to_status_code()
        );
        assert_eq!(
            ("Done".to_owned(), "".to_owned()),
            OTAStatus::Done.to_status_code()
        );
        assert_eq!(
            ("InProgress".to_owned(), "".to_owned()),
            OTAStatus::InProgress.to_status_code()
        );
    }
}
