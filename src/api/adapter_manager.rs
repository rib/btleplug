/// Implements common functionality for adapters across platforms.
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
use crate::api::{BDAddr, CentralEvent, Peripheral};
use dashmap::{mapref::one::RefMut, DashMap};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
#[cfg(feature = "async")]
use {
    futures::channel::mpsc::{self, UnboundedSender},
    futures::stream::Stream,
    std::pin::Pin,
};

#[derive(Clone, Debug)]
pub struct AdapterManager<PeripheralType>
where
    PeripheralType: Peripheral,
{
    peripherals: Arc<DashMap<BDAddr, PeripheralType>>,

    // Sender is never handled mutably, but mpsc's Sender is Send only, not Sync,
    // so we can't just wrap it in an Arc to pass it around as part of the adapter
    // struct. I also don't want to use crossbeam channels, to avoid type leakage.
    //
    // This will be fixed when we go async in 1.0 and can use stream traits. For
    // now, we deal with the lock timing.
    event_sender: Arc<Mutex<Sender<CentralEvent>>>,

    // Normally we'd just return the event receiver when an adapter is created.
    // However, since adapters are cloned and retrieved via lists, this is really
    // hard without changing the fundamentals of the API (which I want to do at
    // some point, but not now). Storing an option here means that we'll only ever
    // have one event receiver (as mpsc isn't clonable, which is what we want on
    // the receiver side anyways), but means we also don't have to deal with the
    // adapter API yet.
    event_receiver: Arc<Mutex<Option<Receiver<CentralEvent>>>>,

    #[cfg(feature = "async")]
    async_senders: Arc<Mutex<Vec<UnboundedSender<CentralEvent>>>>,
}

impl<PeripheralType: Peripheral + 'static> Default for AdapterManager<PeripheralType> {
    fn default() -> Self {
        let peripherals = Arc::new(DashMap::new());
        let (event_sender, event_receiver) = channel();
        AdapterManager {
            peripherals,
            event_sender: Arc::new(Mutex::new(event_sender)),
            event_receiver: Arc::new(Mutex::new(Some(event_receiver))),
            #[cfg(feature = "async")]
            async_senders: Arc::new(Mutex::new(vec![])),
        }
    }
}

impl<PeripheralType> AdapterManager<PeripheralType>
where
    PeripheralType: Peripheral + 'static,
{
    pub fn emit(&self, event: CentralEvent) {
        match event {
            CentralEvent::DeviceDisconnected(addr) => {
                self.peripherals.remove(&addr);
            }
            CentralEvent::DeviceLost(addr) => {
                self.peripherals.remove(&addr);
            }
            _ => {}
        }
        // Since we hold a receiver, this will never fail unless we fill the
        // channel. Whether that's a good idea is another question entirely.
        self.event_sender
            .lock()
            .unwrap()
            .send(event.clone())
            .unwrap();

        #[cfg(feature = "async")]
        // Remove sender from the list if the other end of the channel has been dropped.
        self.async_senders
            .lock()
            .unwrap()
            .retain(|sender| sender.unbounded_send(event.clone()).is_ok());
    }

    pub fn event_receiver(&self) -> Option<Receiver<CentralEvent>> {
        self.event_receiver.lock().unwrap().take()
    }

    #[cfg(feature = "async")]
    pub fn event_stream(&self) -> Pin<Box<dyn Stream<Item = CentralEvent>>> {
        let (sender, receiver) = mpsc::unbounded();
        self.async_senders.lock().unwrap().push(sender);
        Box::pin(receiver)
    }

    pub fn has_peripheral(&self, addr: &BDAddr) -> bool {
        self.peripherals.contains_key(addr)
    }

    pub fn add_peripheral(&self, addr: BDAddr, peripheral: PeripheralType) {
        assert!(
            !self.peripherals.contains_key(&addr),
            "Adding a peripheral that's already in the map."
        );
        assert_eq!(peripheral.address(), addr, "Device has unexpected address."); // TODO remove addr argument
        self.peripherals.insert(addr, peripheral);
    }

    pub fn peripherals(&self) -> Vec<PeripheralType> {
        self.peripherals
            .iter()
            .map(|val| val.value().clone())
            .collect()
    }

    pub fn peripheral_mut(&self, address: BDAddr) -> Option<RefMut<BDAddr, PeripheralType>> {
        self.peripherals.get_mut(&address)
    }

    pub fn peripheral(&self, address: BDAddr) -> Option<PeripheralType> {
        self.peripherals
            .get(&address)
            .map(|val| val.value().clone())
    }
}
