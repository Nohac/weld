//! Application catalogue subscriptions and ordered device commands.
//!
//! Drivers own request handling. UI readers receive immutable catalogue snapshots
//! and bounded completion notifications; transport bindings provide packet halves.

use super::channel::{PacketReceive, PacketSend, receive_record, retain_operation, send_record};
use super::services::ServiceSession;
use super::{CatalogueSubscription, DeviceAction, reply_channel};
use anyhow::{Result, ensure};
use async_channel::{Receiver, Sender};
use futures_lite::future::race;
use std::{
    pin::pin,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use weld_diagnostics::{Operation, Recorder, Report};
use weld_hoist_protocol::device::{ApplicationInfo, DevicePermissions, DeviceReply, DeviceRequest};

const CONTROL_WARNING: Duration = Duration::from_secs(5);

impl ServiceSession {
    pub async fn serve_control(
        &self,
        mut writer: impl PacketSend,
        mut reader: impl PacketReceive,
        permissions: impl Fn() -> Option<DevicePermissions> + Send + Sync,
        exchange_report: impl Fn(Report) -> Option<Report> + Send + Sync,
        recorder: Option<&Recorder>,
    ) -> Result<()> {
        let mut updates: Option<CatalogueSubscription> = None;
        let mut last_collection = None;
        loop {
            // Keep this read pinned while publishing catalogue changes: dropping
            // a partial length/payload read would corrupt the next record.
            let mut reading = pin!(receive_record::<DeviceRequest>(&mut reader));
            let request = loop {
                enum Incoming {
                    Request(Result<DeviceRequest>),
                    Catalogue(Result<super::CatalogueSnapshot>),
                }
                let next = race(async { Incoming::Request(reading.as_mut().await) }, async {
                    Incoming::Catalogue(match &updates {
                        Some(updates) => updates.next().await,
                        None => std::future::pending().await,
                    })
                })
                .await;
                match next {
                    Incoming::Request(request) => break request?,
                    Incoming::Catalogue(snapshot) => {
                        ensure!(self.access.is_authorized(), "desktop access revoked");
                        ensure!(
                            permissions().is_some_and(|grant| grant.browse),
                            "application browsing not permitted"
                        );
                        let reply = DeviceReply::Catalogue(snapshot?.applications.to_vec());
                        retain_operation(
                            CONTROL_WARNING,
                            Operation::SessionWrite,
                            recorder,
                            send_record(&mut writer, &reply),
                        )
                        .await?;
                    }
                }
            };
            ensure!(self.access.is_authorized(), "desktop access revoked");
            let grant = permissions().ok_or_else(|| anyhow::anyhow!("device is not paired"))?;
            let reply = match request {
                DeviceRequest::Subscribe => {
                    ensure!(grant.browse, "application browsing not permitted");
                    updates = Some(self.service.subscribe(self.id).await?);
                    DeviceReply::Accepted(true)
                }
                DeviceRequest::List => {
                    ensure!(grant.browse, "application browsing not permitted");
                    DeviceReply::Catalogue(self.service.catalogue(self.id).await?)
                }
                DeviceRequest::Diagnostics(report) => {
                    let allowed = grant.diagnostics
                        && last_collection
                            .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(1));
                    last_collection = Some(Instant::now());
                    let report = if allowed {
                        exchange_report(*report)
                    } else {
                        None
                    };
                    DeviceReply::Diagnostics(report.map(Box::new))
                }
                DeviceRequest::Hoist(window) => {
                    ensure!(grant.hoist, "hoisting not permitted");
                    let (answer, pending) = reply_channel();
                    self.service
                        .action(DeviceAction::Hoist {
                            session: self.id,
                            window,
                            answer,
                        })
                        .await?;
                    DeviceReply::Accepted(
                        retain_operation(
                            Duration::from_secs(3),
                            Operation::Control,
                            recorder,
                            pending.wait(),
                        )
                        .await?,
                    )
                }
                DeviceRequest::Release => {
                    let (answer, pending) = reply_channel();
                    self.service
                        .action(DeviceAction::Release {
                            session: self.id,
                            answer,
                        })
                        .await?;
                    DeviceReply::Accepted(
                        retain_operation(
                            Duration::from_secs(3),
                            Operation::Control,
                            recorder,
                            pending.wait(),
                        )
                        .await?,
                    )
                }
            };
            ensure!(self.access.is_authorized(), "desktop access revoked");
            retain_operation(
                CONTROL_WARNING,
                Operation::SessionWrite,
                recorder,
                send_record(&mut writer, &reply),
            )
            .await?;
        }
    }
}

type Catalogue = Arc<RwLock<Arc<[ApplicationInfo]>>>;

/// UI-facing commands and published results for one connected device session.
pub struct DeviceClient {
    commands: Sender<DeviceRequest>,
    catalogue: Catalogue,
    diagnostics: Receiver<Option<Report>>,
    errors: Receiver<String>,
}
impl Drop for DeviceClient {
    fn drop(&mut self) {
        self.commands.close();
    }
}
pub struct DeviceClientDriver {
    requests: Receiver<DeviceRequest>,
    catalogue: Catalogue,
    diagnostics: Sender<Option<Report>>,
    errors: Sender<String>,
    notify: Box<dyn Fn() + Send + Sync>,
}
impl DeviceClient {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> (Self, DeviceClientDriver) {
        let (commands, requests) = async_channel::bounded(16);
        let (send_diagnostics, diagnostics) = async_channel::bounded(1);
        let (send_errors, errors) = async_channel::bounded(1);
        let catalogue: Catalogue = Arc::new(RwLock::new(Arc::from([])));
        (
            Self {
                commands,
                catalogue: catalogue.clone(),
                diagnostics,
                errors,
            },
            DeviceClientDriver {
                requests,
                catalogue,
                diagnostics: send_diagnostics,
                errors: send_errors,
                notify: Box::new(notify),
            },
        )
    }
    fn request(&self, request: DeviceRequest) -> Result<()> {
        self.commands
            .try_send(request)
            .map_err(|_| anyhow::anyhow!("device command queue unavailable"))
    }
    pub fn collect_diagnostics(&self, report: Report) -> Result<()> {
        report.validate().map_err(anyhow::Error::msg)?;
        self.request(DeviceRequest::Diagnostics(Box::new(report)))
    }
    pub fn take_diagnostics(&self) -> Option<Option<Report>> {
        self.diagnostics.try_recv().ok()
    }
    pub fn take_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }
    /// The catalogue/control driver still owns its request receiver.
    pub fn is_connected(&self) -> bool {
        !self.commands.is_closed()
    }
    pub fn applications(&self) -> Vec<ApplicationInfo> {
        let snapshot = self
            .catalogue
            .read()
            .map(|snapshot| Arc::clone(&snapshot))
            .unwrap_or_default();
        snapshot.to_vec()
    }
    pub fn hoist(&self, window: u64) -> Result<()> {
        self.request(DeviceRequest::Hoist(window))
    }
    pub fn release(&self) -> Result<()> {
        self.request(DeviceRequest::Release)
    }
}
impl DeviceClientDriver {
    pub async fn run(
        self,
        mut writer: impl PacketSend,
        mut reader: impl PacketReceive,
        recorder: Option<&Recorder>,
    ) -> Result<()> {
        let receiving = async {
            loop {
                match receive_record::<DeviceReply>(&mut reader).await? {
                    DeviceReply::Catalogue(applications) => {
                        ensure!(applications.len() <= 256, "catalogue exceeds bound");
                        ensure!(
                            applications
                                .iter()
                                .all(|app| app.title.len() <= 320 && app.app_id.len() <= 256),
                            "catalogue labels exceed bounds"
                        );
                        let snapshot = applications.into();
                        *self
                            .catalogue
                            .write()
                            .map_err(|_| anyhow::anyhow!("catalogue unavailable"))? = snapshot;
                    }
                    DeviceReply::Diagnostics(report) => {
                        if let Some(report) = &report {
                            report.validate().map_err(anyhow::Error::msg)?;
                        }
                        let _ = self.diagnostics.force_send(report.map(|report| *report));
                    }
                    DeviceReply::Accepted(false) => {
                        let _ = self.errors.force_send(
                            "The application is no longer available for this request".into(),
                        );
                    }
                    DeviceReply::Accepted(true) => {}
                }
                (self.notify)();
            }
        };
        let sending = async {
            retain_operation(
                CONTROL_WARNING,
                Operation::SessionWrite,
                recorder,
                send_record(&mut writer, &DeviceRequest::Subscribe),
            )
            .await?;
            while let Ok(request) = self.requests.recv().await {
                if self.requests.is_closed() {
                    break;
                }
                retain_operation(
                    CONTROL_WARNING,
                    Operation::SessionWrite,
                    recorder,
                    send_record(&mut writer, &request),
                )
                .await?;
            }
            Ok(())
        };
        race(receiving, sending).await
    }
}

#[cfg(test)]
mod tests;
