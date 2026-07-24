//! Host USB inventory, reservations, and per-VM forwarding state.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_api::ArcVec;
use tascarrel_api::types::workspaces as api;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::WorkspaceName;
use tascarrel_vm::HostUsbDevice;
use tascarrel_vm::USB_FORWARDING_PORT_COUNT;
use tascarrel_vm::Vm;
use tascarrel_vm::query_host_usb_devices;
use thiserror::Error;
use tokio::sync::watch;
use tracing::warn;

/// Full-snapshot subscription to connected USB devices and forwarding state.
pub struct UsbDeviceSubscription {
    receiver: watch::Receiver<api::UsbDevicesChangedEvent>,
    initial: bool,
}

impl UsbDeviceSubscription {
    /// Receives the initial inventory or its next changed snapshot.
    pub async fn recv(&mut self) -> Option<api::UsbDevicesChangedEvent> {
        if self.initial {
            self.initial = false;
            return Some(self.receiver.borrow_and_update().clone());
        }
        self.receiver.changed().await.ok()?;
        Some(self.receiver.borrow_and_update().clone())
    }
}

/// Host-wide connected-device inventory and exclusive workspace reservations.
#[derive(Clone)]
pub(crate) struct UsbDeviceRegistry {
    inner: Arc<UsbDeviceRegistryInner>,
}

impl UsbDeviceRegistry {
    /// Creates an empty inventory with the host platform's support status.
    pub(crate) fn new(supported: bool) -> Self {
        let initial = api::UsbDevicesChangedEvent {
            supported,
            devices: ArcVec::new(),
        };
        let (changed, _) = watch::channel(initial);
        Self {
            inner: Arc::new(UsbDeviceRegistryInner {
                state: Mutex::new(UsbDeviceRegistryState {
                    supported,
                    next_id: 1,
                    devices: BTreeMap::new(),
                    reservations: HashMap::new(),
                }),
                changed,
            }),
        }
    }

    /// Opens a subscription whose first event is the current inventory.
    pub(crate) fn subscribe(&self) -> UsbDeviceSubscription {
        UsbDeviceSubscription {
            receiver: self.inner.changed.subscribe(),
            initial: true,
        }
    }

    /// Replaces the observed inventory while preserving continuously connected
    /// identifiers.
    fn refresh(&self, discovered: Vec<ConnectedUsbDevice>) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut previous = std::mem::take(&mut state.devices);
        for device in discovered {
            let existing_id = previous
                .iter()
                .find_map(|(id, previous)| previous.same_connection(&device).then(|| id.clone()));
            let id = existing_id.unwrap_or_else(|| {
                let id = format!("usb-{}", state.next_id);
                state.next_id += 1;
                id
            });
            previous.remove(&id);
            state.devices.insert(id, device);
        }
        self.publish_locked(&state);
    }

    /// Atomically reserves one connected device for a workspace.
    fn reserve(
        &self,
        id: &str,
        workspace: &WorkspaceName,
    ) -> Result<ConnectedUsbDevice, Report<UsbForwardError>> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.supported {
            return Err(UsbForwardError::Unsupported.report());
        }
        let device = state
            .devices
            .get(id)
            .cloned()
            .ok_or(UsbForwardError::Disconnected)
            .report()?;
        if !device.has_required_permissions {
            return Err(UsbForwardError::PermissionDenied.report());
        }
        if let Some(owner) = reservation_owner(&state, id, &device) {
            return Err(UsbForwardError::Busy(owner.clone()).report());
        }
        state.reservations.insert(
            id.to_owned(),
            UsbDeviceReservation {
                workspace: workspace.clone(),
                device: device.clone(),
            },
        );
        self.publish_locked(&state);
        Ok(device)
    }

    fn release(&self, id: &str, workspace: &WorkspaceName) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .reservations
            .get(id)
            .is_some_and(|reservation| &reservation.workspace == workspace)
        {
            state.reservations.remove(id);
            self.publish_locked(&state);
        }
    }

    fn release_workspace(&self, workspace: &WorkspaceName) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_len = state.reservations.len();
        state
            .reservations
            .retain(|_, reservation| &reservation.workspace != workspace);
        if state.reservations.len() != previous_len {
            self.publish_locked(&state);
        }
    }

    fn is_connected(&self, id: &str) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .devices
            .contains_key(id)
    }

    /// Resolves a re-enumerated connection to this workspace's stale
    /// reservation.
    fn owned_reservation_id(&self, id: &str, workspace: &WorkspaceName) -> Option<String> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let device = state.devices.get(id)?;
        state
            .reservations
            .iter()
            .find_map(|(reservation_id, reservation)| {
                (&reservation.workspace == workspace
                    && (reservation_id == id || reservation.device.conflicts_with(device)))
                .then(|| reservation_id.clone())
            })
    }

    /// Publishes a stable, host-address-ordered snapshot after a state
    /// mutation.
    fn publish_locked(&self, state: &UsbDeviceRegistryState) {
        let mut devices = state
            .devices
            .iter()
            .map(|(id, device)| {
                (
                    device.host_bus,
                    device.host_address,
                    api::UsbDevice {
                        id: api::UsbDeviceId::new(id.clone()),
                        name: device.name.clone().into(),
                        manufacturer: device.manufacturer.clone().map(Into::into),
                        product: device.product.clone().map(Into::into),
                        vendor_id: device.vendor_id,
                        product_id: device.product_id,
                        serial_number: device.serial_number.clone().map(Into::into),
                        has_required_permissions: device.has_required_permissions,
                        forwarded_to: reservation_owner(state, id, device).map(api_name),
                    },
                )
            })
            .collect::<Vec<_>>();
        devices.sort_by_key(|(bus, address, _)| (*bus, *address));
        let event = api::UsbDevicesChangedEvent {
            supported: state.supported,
            devices: devices
                .into_iter()
                .map(|(_, _, device)| device)
                .collect::<Vec<_>>()
                .into(),
        };
        if self.inner.changed.borrow().ne(&event) {
            self.inner.changed.send_replace(event);
        }
    }
}

/// USB hotplug state owned by one running managed workspace VM.
pub(crate) struct WorkspaceUsbForwards {
    workspace: WorkspaceName,
    registry: UsbDeviceRegistry,
    forwarded: BTreeMap<String, ForwardedUsbDevice>,
}

impl WorkspaceUsbForwards {
    /// Creates empty forwarding state for one running VM.
    pub(crate) fn new(workspace: WorkspaceName, registry: UsbDeviceRegistry) -> Self {
        Self {
            workspace,
            registry,
            forwarded: BTreeMap::new(),
        }
    }

    /// Reserves and hot-plugs one host device.
    ///
    /// # Errors
    ///
    /// Returns an error when the device cannot be reserved or QEMU rejects it.
    #[tracing::instrument(
        name = "tascarrel_host.workspace_usb.attach",
        level = "info",
        skip(self, vm),
        fields(workspace = %self.workspace, device_id = id),
        err
    )]
    pub(crate) async fn attach(
        &mut self,
        vm: &mut Vm,
        id: &str,
    ) -> Result<(), Report<UsbForwardError>> {
        if self.forwarded.contains_key(id) {
            return Ok(());
        }
        let port = (1..=USB_FORWARDING_PORT_COUNT)
            .find(|port| self.forwarded.values().all(|device| device.port != *port))
            .ok_or(UsbForwardError::PortsExhausted)
            .report()?;
        let device = self.registry.reserve(id, &self.workspace)?;
        let qmp_id = format!("tascarrel-usb-{port}");
        if let Err(error) = vm
            .attach_usb(&qmp_id, device.host_bus, device.host_address, port)
            .await
        {
            self.registry.release(id, &self.workspace);
            return Err(error.escalate(UsbForwardError::Attach));
        }
        self.forwarded
            .insert(id.to_owned(), ForwardedUsbDevice { qmp_id, port });
        Ok(())
    }

    /// Hot-unplugs one device and releases its host-wide reservation.
    ///
    /// # Errors
    ///
    /// Returns an error when this VM does not own the device or QEMU rejects
    /// detachment.
    #[tracing::instrument(
        name = "tascarrel_host.workspace_usb.detach",
        level = "info",
        skip(self, vm),
        fields(workspace = %self.workspace, device_id = id),
        err
    )]
    pub(crate) async fn detach(
        &mut self,
        vm: &mut Vm,
        id: &str,
    ) -> Result<(), Report<UsbForwardError>> {
        let forwarded_id = self
            .forwarded
            .contains_key(id)
            .then(|| id.to_owned())
            .or_else(|| self.registry.owned_reservation_id(id, &self.workspace))
            .filter(|reservation_id| self.forwarded.contains_key(reservation_id))
            .ok_or(UsbForwardError::NotForwarded)
            .report()?;
        let forwarded = self
            .forwarded
            .get(&forwarded_id)
            .expect("the resolved USB reservation is present in this VM");
        vm.detach_usb(&forwarded.qmp_id)
            .await
            .map_err(|error| error.escalate(UsbForwardError::Detach))?;
        self.forwarded.remove(&forwarded_id);
        self.registry.release(&forwarded_id, &self.workspace);
        Ok(())
    }

    /// Detaches devices that are disconnected or no longer enabled by
    /// configuration.
    pub(crate) async fn reconcile(&mut self, vm: &mut Vm, enabled: bool) {
        let stale = self
            .forwarded
            .keys()
            .filter(|id| !enabled || !self.registry.is_connected(id))
            .cloned()
            .collect::<Vec<_>>();
        for id in stale {
            if let Err(error) = self.detach(vm, &id).await {
                warn!(workspace = %self.workspace, device_id = %id, %error, "could not detach stale USB device");
            }
        }
    }

    /// Releases every reservation after the VM has stopped.
    pub(crate) fn release_all(&mut self) {
        self.registry.release_workspace(&self.workspace);
        self.forwarded.clear();
    }
}

/// Caller-relevant failures while changing USB forwarding state.
#[derive(Debug, Error)]
pub(crate) enum UsbForwardError {
    /// The host does not implement USB forwarding.
    #[error("USB forwarding is not supported by this host")]
    Unsupported,
    /// The selected connection disappeared from the live inventory.
    #[error("Selected USB device is no longer connected")]
    Disconnected,
    /// The host daemon cannot open the device's usbfs node.
    #[error("Host daemon lacks permission to access the selected USB device")]
    PermissionDenied,
    /// Another workspace owns the physical connection.
    #[error("Selected USB device is already forwarded to workspace {0}")]
    Busy(WorkspaceName),
    /// Every configured xHCI port is occupied.
    #[error("Workspace VM has no free USB forwarding ports")]
    PortsExhausted,
    /// The selected connection is not owned by this VM.
    #[error("Selected USB device is not forwarded to this workspace")]
    NotForwarded,
    /// QEMU rejected a hot-plug request.
    #[error("QEMU rejected the USB device")]
    Attach,
    /// QEMU rejected a hot-unplug request.
    #[error("QEMU could not detach the USB device")]
    Detach,
}

impl UsbForwardError {
    /// Maps the forwarding failure to the existing workspace-worker protocol.
    pub(crate) const fn remote_code(&self) -> ErrorCode {
        match self {
            Self::Unsupported => ErrorCode::Unsupported,
            Self::Disconnected => ErrorCode::NotFound,
            Self::PermissionDenied => ErrorCode::PermissionDenied,
            Self::Busy(_) => ErrorCode::Busy,
            Self::PortsExhausted => ErrorCode::ResourceExhausted,
            Self::NotForwarded => ErrorCode::InvalidRequest,
            Self::Attach | Self::Detach => ErrorCode::ExecutionFailed,
        }
    }
}

/// Default cadence for reconciling forwarding state with inventory and
/// configuration.
pub(crate) const DEFAULT_USB_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// Runs host discovery until the workspace service shuts down.
pub(crate) async fn run_usb_inventory(
    registry: UsbDeviceRegistry,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(DEFAULT_USB_INVENTORY_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            _ = interval.tick() => {
                match tokio::task::spawn_blocking(query_host_usb_devices).await {
                    Ok(Ok(devices)) => registry.refresh(
                        devices.into_iter().map(ConnectedUsbDevice::from).collect()
                    ),
                    Ok(Err(error)) => {
                        warn!(%error, "could not refresh host USB inventory");
                        registry.refresh(Vec::new());
                    }
                    Err(error) => {
                        warn!(%error, "host USB inventory task failed");
                        registry.refresh(Vec::new());
                    }
                }
            }
        }
    }
}

/// Default cadence for observing physical connection changes.
const DEFAULT_USB_INVENTORY_INTERVAL: Duration = Duration::from_secs(1);
struct UsbDeviceRegistryInner {
    state: Mutex<UsbDeviceRegistryState>,
    changed: watch::Sender<api::UsbDevicesChangedEvent>,
}

struct UsbDeviceRegistryState {
    supported: bool,
    next_id: u64,
    devices: BTreeMap<String, ConnectedUsbDevice>,
    reservations: HashMap<String, UsbDeviceReservation>,
}

struct UsbDeviceReservation {
    workspace: WorkspaceName,
    device: ConnectedUsbDevice,
}

/// One connection snapshot with the host-only QEMU addressing retained.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ConnectedUsbDevice {
    name: String,
    manufacturer: Option<String>,
    product: Option<String>,
    vendor_id: u16,
    product_id: u16,
    serial_number: Option<String>,
    host_bus: u8,
    host_address: u8,
    has_required_permissions: bool,
}

impl From<HostUsbDevice> for ConnectedUsbDevice {
    fn from(device: HostUsbDevice) -> Self {
        Self {
            name: device.name().to_owned(),
            manufacturer: device.manufacturer().map(str::to_owned),
            product: device.product().map(str::to_owned),
            vendor_id: device.vendor_id(),
            product_id: device.product_id(),
            serial_number: device.serial_number().map(str::to_owned),
            host_bus: device.host_bus(),
            host_address: device.host_address(),
            has_required_permissions: device.has_required_permissions(),
        }
    }
}

impl ConnectedUsbDevice {
    fn same_connection(&self, other: &Self) -> bool {
        self.host_bus == other.host_bus
            && self.host_address == other.host_address
            && self.vendor_id == other.vendor_id
            && self.product_id == other.product_id
            && self.serial_number == other.serial_number
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        (self.host_bus == other.host_bus && self.host_address == other.host_address)
            || (self.vendor_id == other.vendor_id
                && self.product_id == other.product_id
                && self.serial_number.is_some()
                && self.serial_number == other.serial_number)
    }
}

#[derive(Debug)]
struct ForwardedUsbDevice {
    qmp_id: String,
    port: u8,
}

fn reservation_owner<'a>(
    state: &'a UsbDeviceRegistryState,
    id: &str,
    device: &ConnectedUsbDevice,
) -> Option<&'a WorkspaceName> {
    state
        .reservations
        .get(id)
        .or_else(|| {
            state
                .reservations
                .values()
                .find(|reservation| reservation.device.conflicts_with(device))
        })
        .map(|reservation| &reservation.workspace)
}

fn api_name(name: &WorkspaceName) -> api::WorkspaceName {
    api::WorkspaceName::new(name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies a physical connection has one owner and reconnecting creates a
    /// new public identity.
    #[test]
    fn reservations_are_exclusive_and_reconnections_get_new_ids() {
        let registry = UsbDeviceRegistry::new(true);
        registry.refresh(vec![device(1, 2, true)]);
        let first_id = registry
            .inner
            .state
            .lock()
            .unwrap()
            .devices
            .first_key_value()
            .unwrap()
            .0
            .clone();
        let alpha = WorkspaceName::new("alpha").unwrap();
        let beta = WorkspaceName::new("beta").unwrap();

        registry.reserve(&first_id, &alpha).unwrap();
        assert!(matches!(
            registry.reserve(&first_id, &beta).unwrap_err().error(),
            UsbForwardError::Busy(owner) if owner == &alpha
        ));

        registry.refresh(Vec::new());
        registry.refresh(vec![device(1, 2, true)]);
        let reconnected_id = registry
            .inner
            .state
            .lock()
            .unwrap()
            .devices
            .first_key_value()
            .unwrap()
            .0
            .clone();
        assert_ne!(reconnected_id, first_id);
        assert!(matches!(
            registry
                .reserve(&reconnected_id, &beta)
                .unwrap_err()
                .error(),
            UsbForwardError::Busy(owner) if owner == &alpha
        ));
        assert_eq!(
            registry.owned_reservation_id(&reconnected_id, &alpha),
            Some(first_id.clone())
        );

        registry.release(&first_id, &alpha);
        registry.reserve(&reconnected_id, &beta).unwrap();
    }

    fn device(
        host_bus: u8,
        host_address: u8,
        has_required_permissions: bool,
    ) -> ConnectedUsbDevice {
        ConnectedUsbDevice {
            name: "Debug probe".to_owned(),
            manufacturer: Some("Example".to_owned()),
            product: Some("Probe".to_owned()),
            vendor_id: 0x1234,
            product_id: 0x5678,
            serial_number: Some("SERIAL".to_owned()),
            host_bus,
            host_address,
            has_required_permissions,
        }
    }
}
