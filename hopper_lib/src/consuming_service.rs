// Copyright (c) 2017-2019, Substratum LLC (https://substratum.net) and/or its affiliates. All rights reserved.

use actix::Recipient;
use actix::Syn;
use live_cores_package::LiveCoresPackage;
use std::borrow::Borrow;
use std::net::SocketAddr;
use std::str::FromStr;
use sub_lib::cryptde::CryptDE;
use sub_lib::cryptde::CryptData;
use sub_lib::cryptde::Key;
use sub_lib::cryptde::PlainData;
use sub_lib::dispatcher::Endpoint;
use sub_lib::dispatcher::InboundClientData;
use sub_lib::hopper::IncipientCoresPackage;
use sub_lib::logger::Logger;
use sub_lib::stream_handler_pool::TransmitDataMsg;

pub struct ConsumingService {
    cryptde: &'static CryptDE,
    _is_bootstrap_node: bool, // TODO: Remember to check this and refuse to consume if set
    to_dispatcher: Recipient<Syn, TransmitDataMsg>,
    to_hopper: Recipient<Syn, InboundClientData>,
    logger: Logger,
}

impl ConsumingService {
    pub fn new(
        cryptde: &'static CryptDE,
        is_bootstrap_node: bool,
        to_dispatcher: Recipient<Syn, TransmitDataMsg>,
        to_hopper: Recipient<Syn, InboundClientData>,
    ) -> ConsumingService {
        ConsumingService {
            cryptde,
            _is_bootstrap_node: is_bootstrap_node,
            to_dispatcher,
            to_hopper,
            logger: Logger::new("ConsumingService"),
        }
    }

    pub fn consume(&self, incipient_cores_package: IncipientCoresPackage) {
        self.logger.debug(format!(
            "Received IncipientCoresPackage with {}-byte payload",
            incipient_cores_package.payload.data.len()
        ));
        let (live_package, next_node_key) =
            LiveCoresPackage::from_incipient(incipient_cores_package, self.cryptde.borrow());

        let encrypted_package = match self.serialize_and_encrypt_lcp(live_package, &next_node_key) {
            Ok(p) => p,
            Err(_) => {
                // TODO what should we do here? (nothing is unbound --so we don't need to blow up-- but we can't send this package)
                return ();
            }
        };

        self.launch_lcp(encrypted_package, next_node_key);
        ()
    }

    fn serialize_and_encrypt_lcp(
        &self,
        live_package: LiveCoresPackage,
        next_node_key: &Key,
    ) -> Result<CryptData, ()> {
        let serialized_package = match serde_cbor::ser::to_vec(&live_package) {
            Ok(package) => package,
            Err(e) => {
                self.logger
                    .error(format!("Couldn't serialize package: {}", e));
                return Err(());
            }
        };

        let encrypted_package = match self
            .cryptde
            .encode(&next_node_key, &PlainData::new(&serialized_package[..]))
        {
            Ok(package) => package,
            Err(e) => {
                self.logger
                    .error(format!("Couldn't encode package: {:?}", e));
                return Err(());
            }
        };
        Ok(encrypted_package)
    }

    fn launch_lcp(&self, encrypted_package: CryptData, next_node_key: Key) {
        if self.cryptde.public_key() == next_node_key {
            self.launch_zero_hop_lcp(encrypted_package);
        } else {
            self.launch_conventional_lcp(encrypted_package, next_node_key);
        }
    }

    fn launch_zero_hop_lcp(&self, encrypted_package: CryptData) {
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678")
                .expect("Something terrible has happened"), // irrelevant
            reception_port: None, // irrelevant
            last_data: false,     // irrelevant
            sequence_number: None,
            is_clandestine: true,
            data: encrypted_package.data,
        };
        self.logger.debug(format!(
            "Sending InboundClientData with {}-byte payload to Hopper",
            inbound_client_data.data.len()
        ));
        self.to_hopper
            .try_send(inbound_client_data)
            .expect("Hopper is dead");
    }

    fn launch_conventional_lcp(&self, encrypted_package: CryptData, next_node_key: Key) {
        let transmit_msg = TransmitDataMsg {
            endpoint: Endpoint::Key(next_node_key),
            last_data: false, // Hopper-to-Hopper streams are never remotely killed
            data: encrypted_package.data,
            sequence_number: None,
        };

        self.logger.debug(format!(
            "Sending TransmitDataMsg with {}-byte payload to Dispatcher",
            transmit_msg.data.len()
        ));
        self.to_dispatcher
            .try_send(transmit_msg)
            .expect("Dispatcher is dead");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix::Actor;
    use actix::Addr;
    use actix::System;
    use hopper::Hopper;
    use std::thread;
    use sub_lib::dispatcher::Component;
    use sub_lib::hopper::ExpiredCoresPackage;
    use sub_lib::peer_actors::BindMessage;
    use sub_lib::route::Route;
    use sub_lib::route::RouteSegment;
    use test_utils::recorder::make_peer_actors_from;
    use test_utils::recorder::make_recorder;
    use test_utils::recorder::Recorder;
    use test_utils::test_utils::cryptde;
    use test_utils::test_utils::zero_hop_route_response;

    #[test]
    fn converts_incipient_message_to_live_and_sends_to_dispatcher() {
        let cryptde = cryptde();
        let dispatcher = Recorder::new();
        let dispatcher_recording_arc = dispatcher.get_recording();
        let dispatcher_awaiter = dispatcher.get_awaiter();
        let destination_key = Key::new(&[65, 65, 65]);
        let route = Route::new(
            vec![RouteSegment::new(
                vec![&cryptde.public_key(), &destination_key.clone()],
                Component::Neighborhood,
            )],
            cryptde,
        )
        .unwrap();
        let payload = PlainData::new(&b"abcd"[..]);
        let incipient_cores_package =
            IncipientCoresPackage::new(route.clone(), payload, &destination_key);
        let incipient_cores_package_a = incipient_cores_package.clone();
        thread::spawn(move || {
            let system = System::new("converts_incipient_message_to_live_and_sends_to_dispatcher");
            let peer_actors = make_peer_actors_from(None, Some(dispatcher), None, None, None, None);
            let subject = Hopper::new(cryptde, false);
            let subject_addr: Addr<Syn, Hopper> = subject.start();
            subject_addr.try_send(BindMessage { peer_actors }).unwrap();

            subject_addr.try_send(incipient_cores_package).unwrap();

            system.run();
        });
        dispatcher_awaiter.await_message_count(1);
        let dispatcher_recording = dispatcher_recording_arc.lock().unwrap();
        let record = dispatcher_recording.get_record::<TransmitDataMsg>(0);
        let expected_lcp = LiveCoresPackage::from_incipient(incipient_cores_package_a, cryptde).0;
        let expected_lcp_ser = PlainData::new(&serde_cbor::ser::to_vec(&expected_lcp).unwrap());
        let expected_lcp_enc = cryptde.encode(&destination_key, &expected_lcp_ser).unwrap();
        assert_eq!(
            *record,
            TransmitDataMsg {
                endpoint: Endpoint::Key(destination_key.clone()),
                last_data: false,
                sequence_number: None,
                data: expected_lcp_enc.data,
            }
        );
    }

    #[test]
    fn hopper_sends_incipient_cores_package_to_recipient_component_when_next_hop_key_is_the_same_as_the_public_key_of_this_node(
    ) {
        let cryptde = cryptde();
        let (component, component_awaiter, component_recording_arc) = make_recorder();
        let destination_key = cryptde.public_key();
        let route = zero_hop_route_response(&cryptde.public_key(), cryptde).route;
        let payload = PlainData::new(&b"abcd"[..]);
        let incipient_cores_package = IncipientCoresPackage::new(route, payload, &destination_key);
        let incipient_cores_package_a = incipient_cores_package.clone();
        let (lcp, _key) = LiveCoresPackage::from_incipient(incipient_cores_package_a, cryptde);
        thread::spawn(move || {
            let system = System::new ("hopper_sends_incipient_cores_package_to_recipient_component_when_next_hop_key_is_the_same_as_the_public_key_of_this_node");
            let mut peer_actors =
                make_peer_actors_from(None, None, None, Some(component), None, None);
            let subject = Hopper::new(cryptde, false);
            let subject_addr: Addr<Syn, Hopper> = subject.start();
            let subject_subs = Hopper::make_subs_from(&subject_addr);
            peer_actors.hopper = subject_subs;
            subject_addr.try_send(BindMessage { peer_actors }).unwrap();

            subject_addr.try_send(incipient_cores_package).unwrap();

            system.run();
        });
        component_awaiter.await_message_count(1);
        let component_recording = component_recording_arc.lock().unwrap();
        let record = component_recording.get_record::<ExpiredCoresPackage>(0);
        let expected_ecp = lcp.to_expired(cryptde);
        assert_eq!(*record, expected_ecp);
    }
}
