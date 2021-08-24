// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

use super::{
    advertisement_data_type, bindings, ble::characteristic::BLECharacteristic,
    ble::device::BLEDevice, utils,
};
use crate::{
    api::{
        bleuuid::{uuid_from_u16, uuid_from_u32},
        AddressType, BDAddr, CentralEvent, Characteristic, Peripheral as ApiPeripheral,
        PeripheralProperties, ValueNotification, WriteType,
    },
    common::{adapter_manager::AdapterManager, util::notifications_stream_from_broadcast_receiver},
    Error, Result,
};
use async_trait::async_trait;
use dashmap::DashMap;
use futures::stream::Stream;
use log::{debug, trace};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    convert::TryInto,
    fmt::{self, Debug, Display, Formatter},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    sync::{Arc, RwLock},
};
use tokio::sync::broadcast;
use uuid::Uuid;

use bindings::Windows::Devices::Bluetooth::Advertisement::*;

/// Implementation of [api::Peripheral](crate::api::Peripheral).
#[derive(Clone)]
pub struct Peripheral {
    shared: Arc<Shared>,
}

struct Shared {
    device: tokio::sync::Mutex<Option<BLEDevice>>,
    adapter: AdapterManager<Peripheral>,
    address: BDAddr,
    connected: AtomicBool,
    ble_characteristics: DashMap<Uuid, BLECharacteristic>,
    notifications_channel: broadcast::Sender<ValueNotification>,

    // Mutable, advertised, state...
    local_name: RwLock<Option<String>>,
    last_tx_power_level: RwLock<Option<i8>>, // FIXME: it's excessive to be locking here
    latest_manufacturer_data: RwLock<HashMap<u16, Vec<u8>>>,
    latest_service_data: RwLock<HashMap<Uuid, Vec<u8>>>,
    services: RwLock<HashSet<Uuid>>,
    discovery_count: AtomicU32,
}

impl Peripheral {
    pub(crate) fn new(adapter: AdapterManager<Self>, address: BDAddr) -> Self {
        let (broadcast_sender, _) = broadcast::channel(16);
        Peripheral {
            shared: Arc::new(Shared {
                adapter: adapter,
                device: tokio::sync::Mutex::new(None),
                address: address,
                connected: AtomicBool::new(false),
                ble_characteristics: DashMap::new(),
                notifications_channel: broadcast_sender,
                local_name: RwLock::new(None),
                last_tx_power_level: RwLock::new(None),
                latest_manufacturer_data: RwLock::new(HashMap::new()),
                latest_service_data: RwLock::new(HashMap::new()),
                services: RwLock::new(HashSet::new()),
                discovery_count: AtomicU32::new(0),
            }),
        }
    }

    fn derive_properties_compat(&self) -> PeripheralProperties {
        PeripheralProperties {
            address: self.address(),
            address_type: self.address_type(),
            local_name: self.local_name(),
            tx_power_level: self.last_tx_power_level(),
            manufacturer_data: self.shared.latest_manufacturer_data.read().unwrap().clone(),
            service_data: self.shared.latest_service_data.read().unwrap().clone(),
            services: self.services(),
            discovery_count: self.shared.discovery_count.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn update_properties(&self, args: &BluetoothLEAdvertisementReceivedEventArgs) {
        let advertisement = args.Advertisement().unwrap();

        //println!("Advertisement received:");
        //println!("Type = {:?}", args.AdvertisementType().unwrap());

        self.shared.discovery_count.fetch_add(1, Ordering::Relaxed);

        // Advertisements are cumulative: set/replace data only if it's set
        if let Ok(name) = advertisement.LocalName() {
            if !name.is_empty() {
                // XXX: we could probably also assume that we've seen the
                // advertisement before and speculatively take a read lock
                // to confirm that the name hasn't changed...

                let mut local_name_guard = self.shared.local_name.write().unwrap();
                *local_name_guard = Some(name.to_string());
            }
        }
        if let Ok(manufacturer_data) = advertisement.ManufacturerData() {
            let mut manufacturer_data_guard = self.shared.latest_manufacturer_data.write().unwrap();

            *manufacturer_data_guard = manufacturer_data
                .into_iter()
                .map(|d| {
                    let manufacturer_id = d.CompanyId().unwrap();
                    let data = utils::to_vec(&d.Data().unwrap());

                    (manufacturer_id, data)
                })
                .collect();

            // Emit event of newly received advertisement
            self.shared
                .adapter
                .emit(CentralEvent::ManufacturerDataAdvertisement {
                    address: self.shared.address,
                    manufacturer_data: manufacturer_data_guard.clone(),
                });
        }

        // The Windows Runtime API (as of 19041) does not directly expose Service Data as a friendly API (like Manufacturer Data above)
        // Instead they provide data sections for access to raw advertising data. That is processed here.
        if let Ok(data_sections) = advertisement.DataSections() {
            // See if we have any advertised service data before taking a lock to update...
            let mut found_service_data = false;
            for section in &data_sections {
                match section.DataType().unwrap() {
                    advertisement_data_type::SERVICE_DATA_16_BIT_UUID
                    | advertisement_data_type::SERVICE_DATA_32_BIT_UUID
                    | advertisement_data_type::SERVICE_DATA_128_BIT_UUID => {
                        found_service_data = true;
                        break;
                    }
                    _ => {}
                }
            }
            if found_service_data {
                let mut service_data_guard = self.shared.latest_service_data.write().unwrap();

                *service_data_guard = data_sections
                    .into_iter()
                    .filter_map(|d| {
                        //let dt = d.DataType().unwrap();
                        //println!("Data type: {:#02x}", dt);
                        let data = utils::to_vec(&d.Data().unwrap());

                        match d.DataType().unwrap() {
                            advertisement_data_type::SERVICE_DATA_16_BIT_UUID => {
                                let (uuid, data) = data.split_at(2);
                                let uuid =
                                    uuid_from_u16(u16::from_le_bytes(uuid.try_into().unwrap()));
                                Some((uuid, data.to_owned()))
                            }
                            advertisement_data_type::SERVICE_DATA_32_BIT_UUID => {
                                let (uuid, data) = data.split_at(4);
                                let uuid =
                                    uuid_from_u32(u32::from_le_bytes(uuid.try_into().unwrap()));
                                Some((uuid, data.to_owned()))
                            }
                            advertisement_data_type::SERVICE_DATA_128_BIT_UUID => {
                                let (uuid, data) = data.split_at(16);
                                let uuid = Uuid::from_slice(uuid).unwrap();
                                Some((uuid, data.to_owned()))
                            }
                            _ => None,
                        }
                    })
                    .collect();

                // Emit event of newly received advertisement
                self.shared
                    .adapter
                    .emit(CentralEvent::ServiceDataAdvertisement {
                        address: self.shared.address,
                        service_data: service_data_guard.clone(),
                    });
            }
        }

        if let Ok(services) = advertisement.ServiceUuids() {
            let mut found_new_service = false;

            // Limited scope for read-only lock...
            {
                let services_guard_ro = self.shared.services.read().unwrap();

                // In all likelihood we've already seen all the advertised services before so lets
                // check to see if we can avoid taking the write lock and emitting an event...
                for uuid in &services {
                    if !services_guard_ro.contains(&utils::to_uuid(&uuid)) {
                        found_new_service = true;
                        break;
                    }
                }
            }

            if found_new_service {
                let mut services_guard = self.shared.services.write().unwrap();

                // ServicesUuids combines all the 16, 32 and 128 bit, 'complete' and 'incomplete'
                // service IDs that may be part of this advertisement into one single list with
                // a consistent (128bit) format. Considering that we don't practically know
                // whether the aggregate list is ever complete we always union the IDs with the
                // IDs already tracked.
                for uuid in services {
                    services_guard.insert(utils::to_uuid(&uuid));
                }

                self.shared
                    .adapter
                    .emit(CentralEvent::ServicesAdvertisement {
                        address: self.shared.address,
                        services: services_guard.iter().map(|uuid| *uuid).collect(),
                    });
            }
        }

        let mut tx_power_level_guard = self.shared.last_tx_power_level.write().unwrap();
        *tx_power_level_guard = args.RawSignalStrengthInDBm().ok().map(|rssi| rssi as i8);
    }
}

impl Display for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let connected = if self.shared.connected.load(Ordering::Relaxed) {
            " connected"
        } else {
            ""
        };
        write!(
            f,
            "{} {}{}",
            self.shared.address,
            self.shared
                .local_name
                .read()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "(unknown)".to_string()),
            connected
        )
    }
}

impl Debug for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let connected = if self.shared.connected.load(Ordering::Relaxed) {
            " connected"
        } else {
            ""
        };
        let properties = self.derive_properties_compat();
        write!(
            f,
            "{} properties: {:?}, characteristics: {:?} {}",
            self.shared.address, properties, self.shared.ble_characteristics, connected
        )
    }
}

#[async_trait]
impl ApiPeripheral for Peripheral {
    /// Returns the address of the peripheral.
    fn address(&self) -> BDAddr {
        self.shared.address
    }

    /// The type of address (either random or public)
    fn address_type(&self) -> Option<AddressType> {
        // windows does not provide the address type in the advertisement event args but only in the device object
        // https://social.msdn.microsoft.com/Forums/en-US/c71d51a2-56a1-425a-9063-de44fda48766/bluetooth-address-public-or-random?forum=wdk
        None
    }

    /// Returns the local name of the device. This is generally a human-readable string that
    /// identifies the type of device. This may be a shortened or complete local name, as defined
    /// by the Bluetooth LE specification.
    fn local_name(&self) -> Option<String> {
        self.shared.local_name.read().unwrap().clone()
    }

    // Returns the most recently advertised TX power level for this device (dBm)
    fn last_tx_power_level(&self) -> Option<i8> {
        *self.shared.last_tx_power_level.read().unwrap()
    }

    /// Advertised services for this device
    fn services(&self) -> Vec<Uuid> {
        self.shared
            .services
            .read()
            .unwrap()
            .iter()
            .map(|uuid_ref| *uuid_ref)
            .collect()
    }

    /// Returns the set of properties associated with the peripheral. These may be updated over time
    /// as additional advertising reports are received.
    async fn properties(&self) -> Result<Option<PeripheralProperties>> {
        Ok(Some(self.derive_properties_compat()))
    }

    /// The set of characteristics we've discovered for this device. This will be empty until
    /// `discover_characteristics` is called.
    fn characteristics(&self) -> BTreeSet<Characteristic> {
        self.shared
            .ble_characteristics
            .iter()
            .map(|item| item.value().to_characteristic())
            .collect()
    }

    /// Returns true iff we are currently connected to the device.
    async fn is_connected(&self) -> Result<bool> {
        Ok(self.shared.connected.load(Ordering::Relaxed))
    }

    /// Creates a connection to the device. This is a synchronous operation; if this method returns
    /// Ok there has been successful connection. Note that peripherals allow only one connection at
    /// a time. Operations that attempt to communicate with a device will fail until it is connected.
    async fn connect(&self) -> Result<()> {
        let shared_clone = self.shared.clone();
        let adapter_clone = self.shared.adapter.clone();
        let address = self.shared.address;
        let device = BLEDevice::new(
            self.shared.address,
            Box::new(move |is_connected| {
                shared_clone
                    .connected
                    .store(is_connected, Ordering::Relaxed);
                if !is_connected {
                    adapter_clone.emit(CentralEvent::DeviceDisconnected(address));
                }
            }),
        )
        .await?;

        device.connect().await?;
        let mut d = self.shared.device.lock().await;
        *d = Some(device);
        self.shared
            .adapter
            .emit(CentralEvent::DeviceConnected(self.shared.address));
        Ok(())
    }

    /// Terminates a connection to the device. This is a synchronous operation.
    async fn disconnect(&self) -> Result<()> {
        let mut device = self.shared.device.lock().await;
        *device = None;
        self.shared
            .adapter
            .emit(CentralEvent::DeviceDisconnected(self.shared.address));
        Ok(())
    }

    /// Discovers all characteristics for the device. This is a synchronous operation.
    async fn discover_characteristics(&self) -> Result<Vec<Characteristic>> {
        let device = self.shared.device.lock().await;
        if let Some(ref device) = *device {
            let mut characteristics_result = vec![];
            let characteristics = device.discover_characteristics().await?;
            for gatt_characteristic in characteristics {
                let ble_characteristic = BLECharacteristic::new(gatt_characteristic);
                let characteristic = ble_characteristic.to_characteristic();
                self.shared
                    .ble_characteristics
                    .entry(characteristic.uuid.clone())
                    .or_insert_with(|| ble_characteristic);
                characteristics_result.push(characteristic);
            }
            return Ok(characteristics_result);
        }
        Err(Error::NotConnected)
    }

    /// Write some data to the characteristic. Returns an error if the write couldn't be send or (in
    /// the case of a write-with-response) if the device returns an error.
    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        if let Some(ble_characteristic) = self.shared.ble_characteristics.get(&characteristic.uuid)
        {
            ble_characteristic.write_value(data, write_type).await
        } else {
            Err(Error::NotSupported("write".into()))
        }
    }

    /// Enables either notify or indicate (depending on support) for the specified characteristic.
    /// This is a synchronous call.
    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        if let Some(mut ble_characteristic) = self
            .shared
            .ble_characteristics
            .get_mut(&characteristic.uuid)
        {
            let notifications_sender = self.shared.notifications_channel.clone();
            let uuid = characteristic.uuid;
            ble_characteristic
                .subscribe(Box::new(move |value| {
                    let notification = ValueNotification { uuid: uuid, value };
                    // Note: we ignore send errors here which may happen while there are no
                    // receivers...
                    let _ = notifications_sender.send(notification);
                }))
                .await
        } else {
            Err(Error::NotSupported("subscribe".into()))
        }
    }

    /// Disables either notify or indicate (depending on support) for the specified characteristic.
    /// This is a synchronous call.
    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        if let Some(mut ble_characteristic) = self
            .shared
            .ble_characteristics
            .get_mut(&characteristic.uuid)
        {
            ble_characteristic.unsubscribe().await
        } else {
            Err(Error::NotSupported("unsubscribe".into()))
        }
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        if let Some(ble_characteristic) = self.shared.ble_characteristics.get(&characteristic.uuid)
        {
            ble_characteristic.read_value().await
        } else {
            Err(Error::NotSupported("read".into()))
        }
    }

    async fn notifications(&self) -> Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        let receiver = self.shared.notifications_channel.subscribe();
        Ok(notifications_stream_from_broadcast_receiver(receiver))
    }
}
